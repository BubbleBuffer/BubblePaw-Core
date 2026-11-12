//! Account attribution follows the actual registry route for every request kind.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse, DecisionUsage,
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, RequestKind,
    RequestUsage, TextCallback, Usage, capture_request_usage, record_request_usage,
};
use bubblepaw_runner::provider::ProviderRegistry;
use std::{sync::Arc, time::Duration};

struct Fixture {
    instrumented: bool,
    fail: bool,
}
impl Fixture {
    fn response(&self, model: &str) -> Result<ModelResponse> {
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Default::default()
        };
        if self.instrumented {
            let mut row = RequestUsage::new("wire-kind", model);
            row.usage = usage.clone();
            row.failed = self.fail;
            record_request_usage(row);
        }
        if self.fail {
            return Err(
                ProviderError::new(ProviderErrorKind::Auth, "fixture failure")
                    .with_usage(usage)
                    .into(),
            );
        }
        Ok(ModelResponse {
            text: "done".into(),
            usage: Some(usage),
            ..Default::default()
        })
    }
}
#[async_trait]
impl Provider for Fixture {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.response(&request.model)
    }
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.response(&request.model)
    }
}
#[async_trait]
impl DecisionProvider for Fixture {
    async fn decide(
        &self,
        request: DecisionRequest,
        _: CancellationToken,
    ) -> Result<DecisionResponse> {
        let response = self.response(&request.model)?;
        Ok(DecisionResponse {
            answers: Vec::new(),
            model: request.model,
            usage: DecisionUsage::from(response.usage.unwrap()),
        })
    }
}

fn request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        session_id: "session".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn account_scopes_cover_main_summary_fork_auxiliary_and_decision_calls() -> Result<()> {
    for instrumented in [false, true] {
        let fixture = Arc::new(Fixture {
            instrumented,
            fail: false,
        });
        let mut registry = ProviderRegistry::new("chat", fixture.clone());
        registry.insert("alias", fixture.clone())?;
        registry.insert_decision("decision", fixture)?;
        registry.set_account("chat", "shared");
        registry.set_account("alias", "shared");
        registry.set_account("decision", "shared");
        assert_eq!(registry.account("chat"), "shared");
        assert_eq!(registry.account("alias/model"), "shared");
        assert_eq!(registry.account("llm/alias/model"), "shared");
        assert_eq!(registry.account("decision/model"), "shared");
        assert_eq!(registry.account("unqualified-model"), "shared");
        let mut rows = Vec::new();
        for step in 0..5 {
            // Keep independent adapter futures on the heap. Combining all five
            // in one captured future exhausts the debug test thread's stack.
            let operation: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>> =
                match step {
                    0 => Box::pin(async {
                        registry
                            .generate(
                                request("chat/model"),
                                Arc::new(|_| {}),
                                CancellationToken::new(),
                            )
                            .await
                            .map(|_| ())
                    }),
                    1 => Box::pin(async {
                        registry
                            .generate_decision(
                                request("alias/model"),
                                Arc::new(|_| {}),
                                CancellationToken::new(),
                            )
                            .await
                            .map(|_| ())
                    }),
                    2 => Box::pin(async {
                        Box::pin(registry.generate_auxiliary(
                            request("chat/model"),
                            CancellationToken::new(),
                            false,
                            Duration::ZERO,
                        ))
                        .await
                        .map(|_| ())
                    }),
                    3 => Box::pin(async {
                        registry
                            .generate_summary_fork(request("alias/model"), CancellationToken::new())
                            .await
                            .map(|_| ())
                    }),
                    _ => Box::pin(async {
                        registry
                            .decide(
                                DecisionRequest {
                                    model: "decision/model".into(),
                                    ..Default::default()
                                },
                                CancellationToken::new(),
                            )
                            .await
                            .map(|_| ())
                    }),
                };
            let (result, captured) =
                capture_request_usage("session", None, RequestKind::Main, operation).await;
            result?;
            rows.extend(captured);
        }
        assert_eq!(rows.len(), 5, "instrumented={instrumented}");
        assert_eq!(
            rows.iter()
                .map(|row| row.provider.as_str())
                .collect::<Vec<_>>(),
            ["chat", "alias", "chat", "alias", "decision"]
        );
        for row in rows {
            assert_eq!(row.account, "shared", "instrumented={instrumented}");
            assert_eq!(row.usage.input_tokens, 12);
        }
    }
    Ok(())
}

#[tokio::test]
async fn fallback_usage_retains_each_actual_account_without_scope_leaking() -> Result<()> {
    let mut registry = ProviderRegistry::new(
        "first",
        Arc::new(Fixture {
            instrumented: true,
            fail: true,
        }),
    );
    registry.insert(
        "second",
        Arc::new(Fixture {
            instrumented: false,
            fail: false,
        }),
    )?;
    registry.set_account("first", "first-account");
    registry.set_account("second", "second-account");
    registry.set_model_fallbacks("first/model".into(), vec!["second/model".into()])?;
    let (result, rows) = capture_request_usage(
        "session",
        None,
        RequestKind::Main,
        registry.generate(
            request("first/model"),
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await;
    result?;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (&*rows[0].provider, &*rows[0].account, rows[0].failed),
        ("first", "first-account", true)
    );
    assert_eq!(
        (&*rows[1].provider, &*rows[1].account, rows[1].failed),
        ("second", "second-account", false)
    );
    assert_eq!(RequestUsage::new("outside", "model").account, "outside");
    Ok(())
}

#[tokio::test]
async fn omitted_account_defaults_to_the_actual_route_name() -> Result<()> {
    let fixture = Arc::new(Fixture {
        instrumented: true,
        fail: false,
    });
    let mut registry = ProviderRegistry::new("default", fixture.clone());
    registry.insert("other", fixture)?;
    let (result, rows) = capture_request_usage(
        "session",
        None,
        RequestKind::Main,
        registry.generate(
            request("other/model"),
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await;
    result?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider, "other");
    assert_eq!(rows[0].account, "other");
    Ok(())
}
