//! Metered loopback relay for replay auxiliary calls.
use super::*;

pub(super) fn replay_roles(
    config: &EpisodeModelConfig,
    prices: &PriceTable,
    decider_enabled: bool,
) -> Result<crate::model_roles::ModelRoles> {
    let roles = config.roles.clone();
    roles.validate()?;
    for (name, role) in [("summary", &roles.summary), ("chapter", &roles.chapter)] {
        for model in &role.models {
            ensure!(
                matches!(model.as_str(), FLASH_MODEL | HAIKU_MODEL) && prices.is_priced(model),
                "Replay metering has no summary/chapter price for {name} model {model}"
            );
        }
    }
    for model in roles.decider.models.iter().filter(|_| decider_enabled) {
        ensure!(
            model.starts_with("openrouter/")
                || prices.is_priced(model)
                || model == "llm/replay/no-main-provider",
            "Replay metering does not support decider {model}"
        );
    }
    Ok(roles)
}
/// The production adapter goes through a loopback-only, single-request HTTP relay.
/// This observes each physical attempt, including retries, without logging headers,
/// credentials, request bodies, response bodies, or reasoning text.
pub(super) async fn relay(
    config: &mut EpisodeModelConfig,
    endpoints: &mut ProviderEndpoints,
    meter: Arc<Mutex<Telemetry>>,
    ledger: PathBuf,
    prices: &'static PriceTable,
    decider_enabled: bool,
) -> Result<(tokio::task::JoinHandle<()>, ProviderKeys)> {
    let read_key = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|key| !key.trim().is_empty())
    };
    let mut keys = ProviderKeys {
        opencode: read_key(&endpoints.opencode_key_env),
        openrouter: if decider_enabled {
            read_key(&endpoints.openrouter_key_env)
        } else {
            None
        },
        openai: if decider_enabled {
            read_key(&endpoints.openai_key_env)
        } else {
            None
        },
    };
    let messages_key = read_key(&endpoints.opencode_messages_key_env);
    let task = relay_transport_keys(
        config,
        endpoints,
        meter,
        ledger,
        RelayPolicy {
            prices,
            decider_enabled,
        },
        &mut keys,
        messages_key,
    )
    .await?;
    Ok((task, keys))
}
#[cfg(test)]
pub(super) async fn relay_configured_keys(
    config: &mut EpisodeModelConfig,
    endpoints: &mut ProviderEndpoints,
    meter: Arc<Mutex<Telemetry>>,
    ledger: PathBuf,
    prices: &'static PriceTable,
    keys: &mut ProviderKeys,
) -> Result<tokio::task::JoinHandle<()>> {
    let messages_key = keys.opencode.clone();
    relay_transport_keys(
        config,
        endpoints,
        meter,
        ledger,
        RelayPolicy {
            prices,
            decider_enabled: true,
        },
        keys,
        messages_key,
    )
    .await
}
struct RelayPolicy {
    prices: &'static PriceTable,
    decider_enabled: bool,
}
async fn relay_transport_keys(
    config: &mut EpisodeModelConfig,
    endpoints: &mut ProviderEndpoints,
    meter: Arc<Mutex<Telemetry>>,
    ledger: PathBuf,
    policy: RelayPolicy,
    keys: &mut ProviderKeys,
    messages_key: Option<String>,
) -> Result<tokio::task::JoinHandle<()>> {
    // Validate upstream settings before replacing them with local relay URLs.
    endpoints.validate()?;
    let RelayPolicy {
        prices,
        decider_enabled,
    } = policy;
    let mut roles = replay_roles(config, prices, decider_enabled)?;
    let summary_key = keys.opencode.clone();
    let available = |model: &str| match model.strip_prefix("llm/").unwrap_or(model) {
        FLASH_MODEL => summary_key.is_some() && endpoints.opencode_base_url.is_some(),
        HAIKU_MODEL => messages_key.is_some() && endpoints.opencode_messages_base_url.is_some(),
        _ => false,
    };
    for role in [&roles.summary, &roles.chapter] {
        ensure!(
            role.models.iter().any(|model| available(model)),
            "Required replay summary/chapter credential or endpoint is unavailable"
        );
    }
    let selected = if decider_enabled {
        Some(
            roles
                .decider
                .models
                .iter()
                .find(|model| {
                    (model.starts_with("openrouter/") && keys.openrouter.is_some())
                        || (model.starts_with("openai/") && keys.openai.is_some())
                        || (model.starts_with("llm/") && available(model))
                })
                .cloned()
                .context("No metered replay decider credential is available")?,
        )
    } else {
        None
    };
    let openai = selected
        .as_deref()
        .is_some_and(|model| model.starts_with("openai/"));
    let router = selected
        .as_deref()
        .is_some_and(|model| model.starts_with("openrouter/"));
    let decider_key = if !decider_enabled {
        None
    } else if openai {
        keys.openai.clone()
    } else if router {
        keys.openrouter.clone()
    } else {
        // Priced LLM decisions use their own Chat or Messages route below.
        summary_key.clone().or_else(|| messages_key.clone())
    };
    if !openai {
        keys.openai = None;
    }
    if !router {
        keys.openrouter = None;
    }
    if let Some(selected) = selected {
        roles.decider.models = vec![selected];
    }
    config.roles = roles;
    let summary_url = endpoints
        .opencode_base_url
        .as_ref()
        .map(|base| format!("{}/chat/completions", base.trim_end_matches('/')));
    let messages_url = endpoints
        .opencode_messages_base_url
        .as_ref()
        .map(|base| format!("{}/messages", base.trim_end_matches('/')));
    let decision_url = if openai {
        let base = endpoints.openai_base_url.trim_end_matches('/');
        format!(
            "{base}{}",
            if base.ends_with("/v1") {
                "/decisions"
            } else {
                "/v1/decisions"
            }
        )
    } else {
        endpoints.openrouter_decisions_url.clone()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    endpoints.opencode_base_url = summary_key
        .as_ref()
        .and(summary_url.as_ref())
        .map(|_| format!("http://{address}/summary"));
    endpoints.opencode_messages_base_url = messages_key
        .as_ref()
        .and(messages_url.as_ref())
        .map(|_| format!("http://{address}/messages"));
    // Registry adapters authenticate only to our loopback relay. Each upstream
    // keeps its own configured credential; these local keys are never forwarded.
    keys.opencode = summary_key.clone().or_else(|| messages_key.clone());
    endpoints.openrouter_decisions_url = format!("http://{address}/decision");
    endpoints.openai_base_url = format!("http://{address}/openai");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    Ok(tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let result: Result<()> = async {
                let mut buffer = Vec::new();
                let (header_end, length) = loop {
                    let mut chunk = [0_u8; 8192];
                    let n = socket.read(&mut chunk).await?;
                    ensure!(n > 0, "Loopback request ended early");
                    buffer.extend_from_slice(&chunk[..n]);
                    ensure!(buffer.len() <= 32 * 1024 * 1024, "Loopback request too large");
                    if let Some(offset) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&buffer[..offset])?;
                        let length = headers.lines().filter_map(|line| line.split_once(':')).find(|(name, _)| name.eq_ignore_ascii_case("content-length")).context("Loopback request lacks length")?.1.trim().parse::<usize>()?;
                        break (offset + 4, length);
                    }
                };
                while buffer.len() < header_end + length {
                    let mut chunk = [0_u8; 8192];
                    let n = socket.read(&mut chunk).await?;
                    ensure!(n > 0 && buffer.len() + n <= 32 * 1024 * 1024, "Invalid loopback body");
                    buffer.extend_from_slice(&chunk[..n]);
                }
                let decision = buffer.starts_with(b"POST /decision ") || buffer.starts_with(b"POST /openai/v1/decisions ");
                ensure!(!decision || decider_enabled, "Replay decider is disabled");
                let messages = buffer.starts_with(b"POST /messages/messages ");
                ensure!(decision || messages || buffer.starts_with(b"POST /summary/chat/completions "), "Unknown replay relay route");
                let body: Value = serde_json::from_slice(&buffer[header_end..header_end + length])?;
                let model_address = if decision { format!("{}/{}", if openai {"openai"} else {"openrouter"}, body["model"].as_str().context("Relay request lacks model")?) } else { format!("{}/{}", if messages {"opencode-messages"} else {"opencode"}, body["model"].as_str().context("Relay request lacks model")?) };
                ensure!(decision && router || prices.is_priced(&model_address), "Replay metering has no price for {model_address}");
                let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
                let estimated_input = tokenizer.encode_ordinary(&body.to_string()).len() as u64;
                // Reserve a complete max-size response before dispatch. Unknown attempts
                // retain this reservation because their provider spend cannot be observed.
                // The OpenAI decider reserves its byte/template input bound; OpenRouter
                // tariffs are known only from reported usage.cost, so their reservation
                // is Flash input at the table rate plus a flat amount.
                let max_output = body["max_tokens"].as_u64().unwrap_or(65536);
                let reservation = if decision && openai {
                    prices.reservation(&model_address, 1_000_000_usize.max(body.to_string().len().saturating_mul(4).saturating_add(262_144)) as u64, max_output).with_context(|| format!("Replay metering has no price for {model_address}"))?
                } else if decision {
                    prices.reservation(FLASH_MODEL, estimated_input, 0).context("Flash price is embedded")? + 0.001
                } else {
                    prices.reservation(&model_address, estimated_input.max(body.to_string().len() as u64), max_output).with_context(|| format!("Replay metering has no price for {model_address}"))?
                };
                let allowed = {
                    let mut meter = meter.lock().unwrap();
                    meter.reserve_next(reservation)
                };
                if !allowed {
                    socket.write_all(b"HTTP/1.1 402 Payment Required\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
                    return Ok(());
                }
                let started = Instant::now();
                let session = body["session_id"].as_str().unwrap_or("recorded-replay");
                let mut request = if decision { client.post(&decision_url).bearer_auth(decider_key.as_ref().context("Decision credential unavailable")?) } else if messages { client.post(messages_url.as_ref().context("Messages endpoint unavailable")?).header("x-api-key", messages_key.as_ref().context("Messages credential unavailable")?).header("anthropic-version", "2023-06-01") } else { client.post(summary_url.as_ref().context("Chat endpoint unavailable")?).bearer_auth(summary_key.as_ref().context("Chat credential unavailable")?) };
                request = request.header(if decision { "x-session-id" } else { "x-opencode-session" }, session);
                let response = request.json(&body).send().await;
                let (status, bytes) = match response {
                    Ok(response) => { let status = response.status().as_u16(); match response.bytes().await { Ok(bytes) => (status, bytes.to_vec()), Err(_) => (502, b"{}".to_vec()) } },
                    Err(_) => (502, b"{}".to_vec()),
                };
                let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let usage = &payload["usage"];
                let cache_read = usage["cache_read_input_tokens"].as_u64().or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64()).or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64()).or_else(|| usage["prompt_cache_hit_tokens"].as_u64()).unwrap_or(0);
                let cache_creation = usage["cache_creation_input_tokens"].as_u64().or_else(|| usage["prompt_tokens_details"]["cache_creation_tokens"].as_u64()).or_else(|| usage["input_tokens_details"]["cache_creation_tokens"].as_u64()).unwrap_or(0);
                let input = usage["prompt_tokens"].as_u64().or_else(|| usage["input_tokens"].as_u64()).and_then(|input| if messages { input.checked_add(cache_read)?.checked_add(cache_creation) } else { Some(input) });
                let output = usage["completion_tokens"].as_u64().or_else(|| usage["output_tokens"].as_u64());
                let reasoning = usage["completion_tokens_details"]["reasoning_tokens"].as_u64().or_else(|| usage["output_tokens_details"]["reasoning_tokens"].as_u64());
                let reported_cost = usage["cost"].as_f64().or_else(|| payload["cost"].as_f64()).filter(|c|c.is_finite() && *c >= 0.0);
                let ok = (200..300).contains(&status) && payload.is_object();
                {
                    let mut meter = meter.lock().unwrap();
                    let transport_retry=meter.observe_attempt(ok);
                    let cost = if decision && router { reported_cost } else { input.zip(output).and_then(|(i,o)| prices.usage_cost(&model_address, &Usage { input_tokens:i, output_tokens:o, cache_read_tokens:cache_read, cache_creation_tokens:cache_creation, reasoning_tokens: None }, None)) };
                    if let Some(cost) = cost { if decision && openai { meter.openai_cost_usd += cost; } else if decision { meter.jev_cost_usd += cost; } else if messages { meter.haiku_cost_usd += cost; } else { meter.flash_cost_usd += cost; } }
                    if cost.is_some_and(|cost| cost > reservation) { meter.stopped = Some("Provider reported cost exceeded the conservative next-call reservation".into()); }
                    // HTTP rejection is not a generated completion. Transport and successful
                    // responses without usage remain uncertain and consume the reservation.
                    if ((200..300).contains(&status) && cost.is_none()) || status == 502 { meter.reserved_unknown_cost_usd += reservation; }
                    if meter.consecutive_failures >= 3 { meter.stopped = Some("Three consecutive model transport/HTTP failures".into()); }
                    if meter.flash_cost_usd + meter.jev_cost_usd + meter.openai_cost_usd + meter.haiku_cost_usd + meter.reserved_unknown_cost_usd >= meter.max_cost_usd { meter.stopped = Some(format!("Measured plus unknown spend reached ${:.6}",meter.max_cost_usd)); }
                    let kind = meter.current_kind.clone(); let operation = meter.current_operation; let attempt = meter.http_attempt;
                    let event=json!({"kind":kind,"model":body["model"],"model_address":model_address,"operation":operation,"physical_call_index":meter.physical_call_index,"request_slot":meter.request_slot,"attempt":attempt,"transport_retry":transport_retry,"outcome":if ok {"ok"} else {"error"},"http_status":status,"input_tokens":input,"output_tokens":output,"cache_read_tokens":cache_read,"cache_creation_tokens":cache_creation,"reasoning_tokens":reasoning,"cost_usd":cost,"latency_ms":started.elapsed().as_millis(),"estimated_input_tokens":estimated_input,"reserved_next_call_cost_usd":reservation});
                    meter.calls.push(event.clone());
                    let mut file=std::fs::OpenOptions::new().create(true).append(true).open(&ledger)?;
                    use std::io::Write;
                    writeln!(file,"{}",serde_json::to_string(&event)?)?;
                    file.sync_all()?;
                }
                // Provider errors are deliberately replaced with a bounded generic body.
                let bytes = if ok { bytes } else { b"{}".to_vec() };
                socket.write_all(format!("HTTP/1.1 {status} Replay\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await?;
                socket.write_all(&bytes).await?;
                Ok(())
            }.await;
            if result.is_err() {
                let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> &'static PriceTable {
        static PRICES: std::sync::OnceLock<PriceTable> = std::sync::OnceLock::new();
        PRICES.get_or_init(PriceTable::embedded)
    }

    #[test]
    fn disabled_decider_skips_only_decider_price_preflight() {
        let mut config = EpisodeModelConfig::default();
        config.roles.decider.models = vec!["openai/unpriced-unused-decider".into()];
        assert!(replay_roles(&config, prices(), false).is_ok());
        assert!(replay_roles(&config, prices(), true).is_err());
        config.roles.summary.models = vec!["custom/unpriced-summary".into()];
        assert!(replay_roles(&config, prices(), false).is_err());
    }

    #[tokio::test]
    async fn disabled_replay_needs_no_decider_key_and_refuses_decision_dispatch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let ledger = directory.path().join("calls.jsonl");
        let meter = Arc::new(Mutex::new(Telemetry::default()));
        let mut config = EpisodeModelConfig::default();
        config.roles.decider.models = vec!["openrouter/unused-decider".into()];
        let mut endpoints = ProviderEndpoints::default();
        let mut keys = ProviderKeys {
            opencode: Some("scripted-summary-key".into()),
            openrouter: None,
            openai: None,
        };
        let failure = relay_transport_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            ledger.clone(),
            RelayPolicy {
                prices: prices(),
                decider_enabled: true,
            },
            &mut keys,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("No metered replay decider credential")
        );
        let proxy = relay_transport_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            ledger.clone(),
            RelayPolicy {
                prices: prices(),
                decider_enabled: false,
            },
            &mut keys,
            None,
        )
        .await?;
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?
            .post(format!("{}/v1/decisions", endpoints.openai_base_url))
            .json(&json!({"model":"unused-decider"}))
            .send()
            .await;
        proxy.abort();
        assert_eq!(response?.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(keys.openai.is_none() && keys.openrouter.is_none());
        assert_eq!(keys.opencode.as_deref(), Some("scripted-summary-key"));
        assert!(meter.lock().unwrap().calls.is_empty());
        assert!(!ledger.exists());
        Ok(())
    }
}
