//! The shell's builders for the loop and for the dispatcher of a run.
use super::*;
use crate::loop_options::LoopOptions;
use crate::observer::Observer;
use crate::run_inputs::{LoopLimits, NewDispatcher, RunLoop};

impl From<&RunOptions> for LoopOptions {
    fn from(options: &RunOptions) -> Self {
        Self {
            model: options.model.clone(),
            max_rounds: options.max_rounds,
            max_output_tokens: options.max_output_tokens,
            max_tool_calls: options.max_tool_calls,
            output_schema: options.output_schema.clone(),
            script: options.script.clone(),
        }
    }
}

impl Harness {
    /// The config's observation as the watcher the loop and the dispatcher ask.
    pub(super) fn observer(&self) -> Option<Arc<dyn Observer>> {
        self.config
            .observation
            .clone()
            .map(|observation| Arc::new(observation) as Arc<dyn Observer>)
    }
    /// The loop over this harness's journal, live sink, request context, provider
    /// binding and limits.
    pub(super) fn run_loop(&self) -> RunLoop {
        let provider = self.config.provider_resolver.clone().unwrap_or_else(|| {
            let (model, provider) = (self.config.model.clone(), self.provider.clone());
            Arc::new(move || (model.clone(), provider.clone()))
        });
        RunLoop {
            journal: self.journals.run.clone(),
            live: self.live.clone(),
            request_context: self.request_context.clone(),
            provider,
            observation: self.observer(),
            limits: LoopLimits {
                turn_start: self.config.turn_start.clone(),
                max_rounds: self.config.max_rounds,
                max_request_bytes: self.config.max_request_bytes,
                max_paragraph_bytes: self.config.max_paragraph_bytes,
            },
        }
    }
    /// The dispatcher a run starts with over its first tools: the config's approval
    /// policy, the run's override, its tool budget and whether the user may be asked.
    pub(super) fn run_dispatcher(
        &self,
        options: &RunOptions,
        single_tool: bool,
    ) -> Box<NewDispatcher> {
        let (journal, live, gate) = (
            self.journals.run.clone(),
            self.live.clone(),
            self.stock_gate(),
        );
        let observation = self.observer();
        let max_result_bytes = self.config.max_tool_result_bytes;
        let setup = crate::ApprovalPolicy::context_setup(
            self.config.approval.clone(),
            self.config
                .run_approval_resolver(options.approval.clone(), single_tool),
        );
        let budget = options.max_tool_calls;
        Box::new(move |tools| {
            ToolDispatcher::from_seams(
                tools,
                max_result_bytes,
                observation.clone(),
                journal.clone(),
                gate.clone(),
                live.clone(),
                setup.clone(),
            )
            .with_tool_budget(budget)
            .with_interactive(!single_tool)
        })
    }
}
