//! Shared admission, invocation and effect journaling for every tool route.
use super::*;

/// A call admitted against one registry and policy snapshot. The same admission
/// is used by native calls, composition wrappers and model-free host calls.
pub(crate) struct PreparedToolCall {
    context: ToolContext,
    call: ToolCall,
    tool: Option<Arc<dyn crate::Tool>>,
    rejection: Option<ToolResult>,
    approved: bool,
    validation_rejected: bool,
}

pub(crate) struct DispatchedToolCall {
    pub result: ToolResult,
    pub invocation: Option<String>,
}

enum CallJournal<'a> {
    Model {
        message_id: &'a str,
        invocation: Option<String>,
    },
    Nested,
}

impl ToolDispatcher {
    /// Prepare against the effective registry and reserve the shared run budget.
    /// Validation precedes approval and every durable effect boundary.
    pub(crate) fn prepare_call(
        &self,
        mut context: ToolContext,
        call: ToolCall,
        tools_open: bool,
    ) -> Result<PreparedToolCall> {
        ensure!(!context.cancellation.is_cancelled(), "Execution cancelled");
        context.approval = self.approval_policy(&context.session_id);
        let validation = self.tools.validate_model_call(&call).and_then(|()| {
            ensure!(
                serde_json::to_vec(&call.arguments)?.len() <= 2 * 1024 * 1024,
                "Tool input exceeds size limit"
            );
            Ok(())
        });
        // Structured completion must remain reachable after work exhausts the
        // budget, including through JS/call_tool. Their ordinary nested targets
        // still reserve from the same exhausted counter and are refused.
        let terminal = self.tools.get(crate::submit_result::NAME).is_some()
            && (call.name == crate::submit_result::NAME
                || crate::tool_views::is_access_wrapper(&call.name));
        let budget_allowed = (tools_open || terminal) && (self.reserve_tool_call() || terminal);
        let validation_rejected = budget_allowed && validation.is_err();
        let rejection = if !budget_allowed {
            Some(ToolResult::error(
                if self.remaining_tool_calls() == Some(0) {
                    "Tool call budget exhausted; this call was not executed"
                } else {
                    "Tool calls are unavailable for the rest of this run; this call was not executed"
                },
            ))
        } else {
            validation
                .err()
                .map(|error| ToolResult::error(error.to_string()))
        };
        let tool = self.tools.get(&call.name);
        Ok(PreparedToolCall {
            context,
            call,
            tool,
            rejection,
            approved: false,
            validation_rejected,
        })
    }
    /// Admit an unconditional Allow call without an intervening approval write,
    /// permitting the reply and dispatch boundary to share one transaction.
    pub(crate) fn admit_fast_path(&self, prepared: &mut PreparedToolCall) -> Result<bool> {
        if prepared.rejection.is_some() {
            return Ok(false);
        }
        let tool = prepared.tool.as_ref().expect("validated tool");
        let policy = &prepared.context.approval;
        let workspaces = self.workspace_roots(&prepared.context.session_id)?;
        let allowed = policy.mode(&prepared.call.name, tool.definition().approval)
            == crate::ApprovalMode::Allow
            && policy
                .hardline_match(&prepared.call.name, &prepared.call.arguments, &workspaces)
                .is_none();
        prepared.approved = allowed;
        Ok(allowed)
    }
    pub(crate) fn commit_reply_dispatch(
        &self,
        message: &crate::Message,
        usage: &[crate::RequestUsage],
        request_ids: &[String],
        prepared: &PreparedToolCall,
    ) -> Result<String> {
        ensure!(
            prepared.approved && prepared.rejection.is_none(),
            "Call is not admitted"
        );
        ensure!(
            !prepared.context.cancellation.is_cancelled(),
            "Cancelled before tool dispatch"
        );
        self.log.commit_reply(
            &self.store,
            &prepared.context.session_id,
            &prepared.context.run_id,
            None,
            |tx| {
                Store::insert_message(tx, message)?;
                Store::insert_message_request_usage_tx(tx, message, usage, request_ids)?;
                let invocation = Store::dispatch_tx(
                    tx,
                    &prepared.context.session_id,
                    &prepared.context.run_id,
                    &prepared.call,
                )?;
                Ok((
                    invocation,
                    vec![(
                        EventKind::ToolStarted {
                            call: prepared.call.clone(),
                        },
                        Some(crate::event_log::ContentRef::ToolCall {
                            message_id: message.id.clone(),
                            call_id: prepared.call.id.clone(),
                        }),
                    )],
                ))
            },
        )
    }
    /// Invoke a target through the shared validation, budget, approval and effect
    /// path. Full nested results remain inside the host; the enclosing model call
    /// applies the model reply limit.
    pub async fn invoke(
        &self,
        context: ToolContext,
        name: &str,
        input: Value,
    ) -> Result<ToolResult> {
        let prepared = self.prepare_call(
            context,
            ToolCall {
                id: id(),
                name: name.into(),
                arguments: input,
            },
            true,
        )?;
        Ok(self
            .invoke_prepared(prepared, CallJournal::Nested)
            .await?
            .result)
    }
    pub(crate) async fn invoke_direct(
        &self,
        prepared: PreparedToolCall,
        message_id: &str,
        invocation: Option<String>,
    ) -> Result<DispatchedToolCall> {
        self.invoke_prepared(
            prepared,
            CallJournal::Model {
                message_id,
                invocation,
            },
        )
        .await
    }
    async fn invoke_prepared(
        &self,
        mut prepared: PreparedToolCall,
        journal: CallJournal<'_>,
    ) -> Result<DispatchedToolCall> {
        let nested = matches!(journal, CallJournal::Nested);
        let mut observation = self.observe_tool(
            &prepared.context,
            prepared.call.id.clone(),
            &prepared.call.name,
            &prepared.call.arguments,
            nested,
        );
        let result = self
            .execute_prepared(&mut prepared, journal, &mut observation)
            .await;
        if let Some(observation) = &mut observation {
            match &result {
                Ok(dispatched) => observation.finish(&Ok(dispatched.result.clone())),
                Err(error) => observation.finish(&Err(anyhow!(error.to_string()))),
            }
        }
        result
    }
    async fn execute_prepared(
        &self,
        prepared: &mut PreparedToolCall,
        journal: CallJournal<'_>,
        observation: &mut Option<crate::observation::ToolObservation>,
    ) -> Result<DispatchedToolCall> {
        ensure!(
            !prepared.context.cancellation.is_cancelled(),
            "Execution cancelled"
        );
        if prepared.rejection.is_none() && !prepared.approved {
            let message_id = match &journal {
                CallJournal::Model { message_id, .. } => Some(*message_id),
                CallJournal::Nested => None,
            };
            prepared.rejection = self
                .approve_source(&prepared.context, &prepared.call, message_id)
                .await?
                .denial("Execution cancelled")?
                .map(ToolResult::error);
        }
        if let Some(result) = prepared.rejection.take() {
            // Invalid model calls retain their ordinary correctable reply event,
            // but never create an effect. Approval and budget refusals do not start.
            if prepared.validation_rejected
                && let CallJournal::Model { message_id, .. } = journal
            {
                self.start_model_call(&prepared.context, &prepared.call, message_id, false)?;
            }
            return Ok(DispatchedToolCall {
                result,
                invocation: None,
            });
        }
        let tool = prepared.tool.take().expect("validated tool");
        let nested = matches!(journal, CallJournal::Nested);
        let durable = !nested || !tool.definition().read_only;
        let invocation = match journal {
            CallJournal::Model {
                message_id,
                invocation,
            } => Some(match invocation {
                Some(invocation) => invocation,
                None => self
                    .start_model_call(&prepared.context, &prepared.call, message_id, true)?
                    .expect("dispatched invocation"),
            }),
            CallJournal::Nested => {
                let invocation = if durable {
                    Some(self.store.dispatch_nested(
                        &prepared.context.session_id,
                        &prepared.context.run_id,
                        &prepared.call,
                    )?)
                } else {
                    None
                };
                self.log.transient(
                    &prepared.context.session_id,
                    &prepared.context.run_id,
                    EventKind::ToolStarted {
                        call: prepared.call.clone(),
                    },
                );
                invocation
            }
        };
        prepared.context.invocation_id = invocation.clone().unwrap_or_else(id);
        if let Some(observation) = observation {
            observation.invocation(&prepared.context.invocation_id);
        }
        let result = match self
            .call(
                prepared.context.clone(),
                &prepared.call.name,
                tool,
                prepared.call.arguments.clone(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                if let Some(invocation) = &invocation {
                    self.store.effect_unknown(invocation)?;
                }
                return Err(error);
            }
        };
        let result = if nested {
            if let Some(invocation) = &invocation
                && !result.pending
            {
                self.store.effect_completed(invocation)?;
            }
            self.log.transient(
                &prepared.context.session_id,
                &prepared.context.run_id,
                EventKind::ToolFinished {
                    call_id: prepared.call.id.clone(),
                    name: prepared.call.name.clone(),
                    result: result.clone(),
                },
            );
            result
        } else {
            self.limit_model_result(&prepared.context, result)?
        };
        Ok(DispatchedToolCall { result, invocation })
    }
    fn start_model_call(
        &self,
        context: &ToolContext,
        call: &ToolCall,
        message_id: &str,
        valid: bool,
    ) -> Result<Option<String>> {
        self.log.commit(
            &self.store,
            &context.session_id,
            &context.run_id,
            None,
            |tx| {
                let invocation = if valid {
                    Some(Store::dispatch_tx(
                        tx,
                        &context.session_id,
                        &context.run_id,
                        call,
                    )?)
                } else {
                    None
                };
                Ok((
                    invocation,
                    vec![(
                        EventKind::ToolStarted { call: call.clone() },
                        Some(crate::event_log::ContentRef::ToolCall {
                            message_id: message_id.into(),
                            call_id: call.id.clone(),
                        }),
                    )],
                ))
            },
        )
    }
    /// Complete the direct reply and effect in the same transaction.
    pub(crate) fn record_direct_result(
        &self,
        run_id: &str,
        message: &crate::Message,
        call: &ToolCall,
        result: ToolResult,
        invocation: Option<&str>,
    ) -> Result<()> {
        self.log
            .commit(&self.store, &message.session_id, run_id, None, |tx| {
                Store::tool_completed_tx(tx, message, invocation, result.pending)?;
                Ok((
                    (),
                    vec![(
                        EventKind::ToolFinished {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            result,
                        },
                        Some(crate::event_log::ContentRef::ToolResult {
                            message_id: message.id.clone(),
                        }),
                    )],
                ))
            })
    }
    async fn call(
        &self,
        context: ToolContext,
        name: &str,
        tool: Arc<dyn crate::Tool>,
        input: Value,
    ) -> Result<ToolResult> {
        let cancellation: CancellationToken = context.cancellation.clone();
        let mut child = self.clone();
        // Origin is diagnostic only; all targets still pass the same gates.
        child.origin = match name {
            "js" => "js",
            "call_tool" => "call_tool",
            _ => self.origin,
        };
        let result = tokio::select! {biased;_=cancellation.cancelled()=>return Err(anyhow!("Cancelled during tool; outcome unknown")), result=tool.invoke_with_dispatcher(context,input,child)=>match result{Ok(value)=>value,Err(error)=>ToolResult::error(error.to_string())}};
        Ok(result)
    }
}
