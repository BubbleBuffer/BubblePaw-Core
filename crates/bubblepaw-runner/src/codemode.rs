//! Volatile, session-owned QuickJS environments. Every cell has a fresh async
//! function scope; only `state` deliberately survives normal completion. There
//! is no heap checkpoint/replay. Cancellation and resource failures replace the
//! heap and increment its generation, because abandoned continuations cannot
//! safely share an environment with a subsequent cell.
//!
//! Each environment owns one OS thread and one bounded QuickJS runtime. Host
//! calls run on Tokio and return through a channel; the JS thread never blocks
//! on a Rust future. Explicit `wait` calls suspend the idle deadline, but every
//! JS job and the cumulative active JS time remain bounded. Backgrounding keeps
//! the original cell, cancellation token, dispatcher and output owner alive.
use crate::artifacts::ArtifactStore;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
    search_tool_definitions_with_servers, tool_definition_summary, track_approval_waits,
};
use rquickjs::{Context, Ctx, Function, Object, Persistent, Promise, Runtime, Value as JsValue};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tiktoken_rs::{CoreBPE, bpe_for_tokenizer, tokenizer::Tokenizer};
use tokio::sync::{Notify, oneshot};

#[path = "codemode_reference.rs"]
mod reference;
use reference::js_reference;

const MAX_ENVS: usize = 16;
const MAX_SESSION_ENVS: usize = 4;
const MAX_CODE: usize = 128 * 1024;
const MAX_RETURN_BYTES: usize = 60 * 1024;
const MAX_SERIALIZED_RETURN_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_OUTPUT_TOKENS: usize = 10_000;
const MIN_OUTPUT_TOKENS: usize = 256;
const MAX_NOTICE_BYTES: usize = 2048;
const MAX_OUTPUTS: usize = 64;
const MAX_OUTPUT_BYTES: usize = 192 * 1024;
const MAX_LOG_LINES: usize = 256;
const MAX_LOG_BYTES: usize = 16 * 1024;
const MAX_PENDING: usize = 64;
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const JS_BUDGET: Duration = Duration::from_secs(3);
const IDLE_LIMIT: Duration = Duration::from_secs(30);

fn output_tokens(input: &Value) -> Result<usize> {
    match input.get("maxOutputTokens") {
        None => Ok(DEFAULT_OUTPUT_TOKENS),
        Some(value) => {
            let tokens = value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .context("maxOutputTokens must be an integer of at least 256")?;
            ensure!(
                tokens >= MIN_OUTPUT_TOKENS,
                "maxOutputTokens must be an integer of at least 256"
            );
            Ok(tokens)
        }
    }
}

/// Model previews are separate from the successful JS return and its heap.
/// Spills are session-owned data-directory text files, never UI publications.
struct OutputBudget {
    spills: crate::spill::SpillStore,
    tokenizer: &'static CoreBPE,
}
impl OutputBudget {
    fn new(spills: crate::spill::SpillStore) -> Result<Self> {
        Ok(Self {
            spills,
            tokenizer: bpe_for_tokenizer(Tokenizer::O200kBase)?,
        })
    }
    fn fits(&self, content: &str, tokens: usize) -> bool {
        content.len() <= MAX_RETURN_BYTES && self.tokenizer.encode_ordinary(content).len() <= tokens
    }
    fn limit(&self, session: &str, content: &str, notices: &[String], tokens: usize) -> String {
        self.limit_inner(session, content, notices, tokens, None)
    }
    fn cell_reply(
        &self,
        session: &str,
        value: Value,
        notices: &[String],
        tokens: usize,
        view: &View,
    ) -> String {
        if view.logs.is_empty() && view.logs_omitted == 0 {
            return self.limit(session, &value.to_string(), notices, tokens);
        }
        let mut content = json!({"result":value,"logs":view.logs,"logs_omitted":view.logs_omitted});
        // Drop complete lines, never partial JSON or a partial log line. The
        // omission count survives even when the result itself needs a preview.
        while !self.fits(&with_state_notices(content.to_string(), notices), tokens) {
            if content["logs"].as_array_mut().unwrap().pop().is_none() {
                break;
            }
            content["logs_omitted"] = json!(content["logs_omitted"].as_u64().unwrap() + 1);
        }
        self.limit_inner(
            session,
            &content.to_string(),
            notices,
            tokens,
            Some(&content),
        )
    }
    fn limit_inner(
        &self,
        session: &str,
        content: &str,
        notices: &[String],
        tokens: usize,
        logs: Option<&Value>,
    ) -> String {
        let full = with_state_notices(content.to_owned(), notices);
        if self.fits(&full, tokens) {
            return full;
        }
        let mut receipt = json!({"truncated":true,"previewFormat":"text","preview":"",
            "stateChanges":notices,"metadataTruncated":false});
        if let Some(logs) = logs {
            receipt["logs"] = logs["logs"].clone();
            receipt["logs_omitted"] = logs["logs_omitted"].clone();
        }
        // Keep process handles and recovery cursors visible when the returned value spills.
        if let Ok(value) = serde_json::from_str::<Value>(content) {
            for key in ["handle", "status", "cursor", "exitCode"] {
                if let Some(value) = value.get(key)
                    && value.to_string().len() <= 1024
                {
                    receipt[key] = value.clone();
                }
            }
        }
        match self.spills.save(session, &full) {
            Ok(reference) => receipt["fullOutput"] = reference,
            Err(error) => {
                receipt["fullOutputError"] = json!(
                    format!("Could not save full output: {error}")
                        .chars()
                        .take(200)
                        .collect::<String>()
                )
            }
        }
        // Metadata shares the same budget. Preserve early notices (including
        // reset notices) and explicitly disclose omitted notice metadata.
        while !self.fits(&receipt.to_string(), tokens) {
            let notices = receipt["stateChanges"].as_array_mut().unwrap();
            if notices.pop().is_none() {
                break;
            }
            receipt["metadataTruncated"] = json!(true);
        }
        if !self.fits(&receipt.to_string(), tokens) {
            // Only a spill failure can make the fixed receipt large. Keep the
            // failure explicit, even at the smallest accepted budget.
            receipt["fullOutputError"] = json!("Could not save full output");
            receipt["metadataTruncated"] = json!(true);
        }
        let chars = content.chars().collect::<Vec<_>>();
        let mut low = 0;
        let mut high = chars.len().min(MAX_RETURN_BYTES);
        let mut best = receipt.to_string();
        while low < high {
            let count = low + (high - low).div_ceil(2);
            let head = count.div_ceil(2);
            let tail = count / 2;
            receipt["preview"] = json!(format!(
                "{}\n[... output preview truncated ...]\n{}",
                chars[..head].iter().collect::<String>(),
                chars[chars.len() - tail..].iter().collect::<String>()
            ));
            let candidate = receipt.to_string();
            if self.fits(&candidate, tokens) {
                low = count;
                best = candidate;
            } else {
                high = count - 1;
            }
        }
        best
    }
}

// Native functions are enclosed, never installed on the global object. Cell
// helpers capture their execution ID: saved callbacks cannot emit as a later
// cell. The descriptor-based state preview does not call getters/toJSON.
const BOOTSTRAP: &str = r#"(function(enqueue, emit) {
 'use strict';
 const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
 const parse = JSON.parse, stringify = JSON.stringify, descriptors = Object.getOwnPropertyDescriptors;
 const keys = Object.keys, create = Object.create, freeze = Object.freeze;
 const ownKeys = Reflect.ownKeys, getPrototype = Object.getPrototypeOf, setPrototype = Object.setPrototypeOf;
 const objectPrototype = Object.prototype, arrayPrototype = Array.prototype, isArray = Array.isArray;
 const slice = Function.prototype.call.bind(String.prototype.slice), finite = Number.isFinite;
 const trimEnd = Function.prototype.call.bind(String.prototype.trimEnd), endsWith = Function.prototype.call.bind(String.prototype.endsWith);
 const state = create(null), pending = new Map(), identities = new WeakMap();
 let identitySerial = 0, resetSerial = 0, beforeState;
 const identity = value => {
   if (value === undefined) return 0;
   if (!identities.has(value)) identities.set(value,++identitySerial);
   return identities.get(value);
 };
 Object.defineProperty(state,'reset',{value:()=>{
   const ds=descriptors(state), names=ownKeys(ds).filter(key=>key!=='reset');
   for (const key of names) if (!ds[key].configurable)
     throw new TypeError('state.reset cannot clear a non-configurable property; no keys were deleted');
   for (const key of names) delete state[key];
   resetSerial++;
 }});
 function stateSnapshot() {
   let remaining=2048, limited=false;
   const ancestors=new Set();
   function capture(value,depth) {
     const type=typeof value;
     if (--remaining<0 || depth>16) { limited=true; return ['limited',type]; }
     if (value===null || (type!=='object' && type!=='function')) return [type,value];
     const id=identity(value);
     if (ancestors.has(value)) return ['cycle',id];
     const prototype=getPrototype(value);
     if (prototype!==null && prototype!==objectPrototype && prototype!==arrayPrototype) limited=true;
     ancestors.add(value);
     const ds=descriptors(value), names=ownKeys(ds), entries=[];
     if (names.length>64) limited=true;
     for (let i=0;i<names.length && i<64;i++) { const key=names[i];
       const d=ds[key];
       entries.push([key,d.configurable,d.enumerable,d.writable,
         'value' in d ? capture(d.value,depth+1) : ['accessor',identity(d.get),identity(d.set)]]);
     }
     ancestors.delete(value);
     return [type,id,names.length,entries];
   }
   const ds=descriptors(state), names=ownKeys(ds).filter(key=>key!=='reset'), entries=new Map();
   if (names.length>64) limited=true;
   for (let i=0;i<names.length && i<64;i++) { const key=names[i];
     const d=ds[key];
     entries.set(key,[d.configurable,d.enumerable,d.writable,
       'value' in d ? capture(d.value,0) : ['accessor',identity(d.get),identity(d.set)]]);
   }
   return {entries,limited,names:new Set(names),reset:resetSerial};
 }
 function sameSnapshot(a,b) {
   if (Object.is(a,b)) return true;
   if (!isArray(a) || !isArray(b) || a.length!==b.length) return false;
   for (let i=0;i<a.length;i++) if (!sameSnapshot(a[i],b[i])) return false;
   return true;
 }
 function stateNotices() {
   const after=stateSnapshot(), changes=[];
   const label=key=>{
     if (typeof key==='symbol') return 'state[symbol]';
     const short=slice(key,0,80);
     return (/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(short) ? 'state.'+short : 'state['+stringify(short)+']')+(key.length>80?' (key truncated)':'');
   };
   if (beforeState.reset!==after.reset) changes.push('state reset');
   for (const [key,value] of after.entries) {
     if (!beforeState.names.has(key)) changes.push(label(key)+' added');
     else if (beforeState.entries.has(key) && !sameSnapshot(value,beforeState.entries.get(key))) changes.push(label(key)+' changed');
   }
   for (const key of beforeState.entries.keys()) if (!after.names.has(key)) changes.push(label(key)+' deleted');
   if (beforeState.limited || after.limited) changes.push('state observation limited; some nested or additional key changes may be unobserved');
   return stringify(changes);
 }
 // Experimental: keep interpolation values separate until Rust validates the
 // argument placement and chooses the host's command encoding. Raw template
 // text preserves shell backslashes instead of applying JS string escapes.
 const cmd = (parts, ...values) => {
   if (!Array.isArray(parts) || !Array.isArray(parts.raw) || parts.length !== values.length + 1)
     throw new TypeError('cmd must be used as a tagged template');
   const args = values.map(value => {
     const type = typeof value;
     if (type !== 'string' && type !== 'boolean' && !(type === 'number' && finite(value)))
       throw new TypeError('cmd substitutions must be strings, finite numbers, or booleans');
     return String(value);
   });
   return freeze({parts:freeze(Array.from(parts.raw)),values:freeze(args)});
 };
 let serial = 0;
 function call(execution, name, args) {
   const id = String(++serial);
   return new Promise((resolve,reject) => {
     pending.set(id,{resolve,reject});
     try { enqueue(execution,id,name,stringify(args === undefined ? {} : args)); }
     catch(e) { pending.delete(id); reject(e); }
   });
 }
 return {
   execute(execution, code, encodedNames) {
     beforeState=stateSnapshot();
     const exposed = create(null);
     for (const name of parse(encodedNames)) exposed[name] = args => call(execution,name,args);
     exposed.$search = (query, options) => call(execution,'#search',{query, limit: options === undefined || options === null ? undefined : options.limit});
     exposed.$list = () => call(execution,'#list',{});
     // Enumerated properties are the effective capability snapshot. An
     // explicitly requested hidden/unknown target still reaches the common
     // dispatcher, so every execution adapter returns the same policy error.
     const tools = new Proxy(freeze(exposed), {get(target,name) {
       if (Object.prototype.hasOwnProperty.call(target,name)) return target[name];
       if (typeof name !== 'string' || name === 'then' || name === 'js' || name === 'tool_search' || name === 'call_tool') return undefined;
       return args => call(execution,name,args);
     }});
     const text = value => emit(execution,stringify({type:'text',text:String(value)}));
     const file = args => call(execution,'#file',args);
     const image = args => call(execution,'#image',args);
     const wait = args => call(execution,'wait',args);
     const log = (...args) => {
       const line = args.map(value => typeof value === 'string' ? value :
         (stringify(value) ?? 'undefined')).join(' ');
       emit(execution,stringify({type:'log',text:line.length > 16384 ? null : line}));
     };
     const console = freeze({log,info:log,warn:log,error:log});
     const parameters = ['state','tools','text','file','image','wait','cmd','console'];
     let body, bodyError;
     // Validate without injected parameters, then give the user body its own
     // function scope. Locals may shadow helpers; saved closures still capture
     // this cell's helpers and cannot acquire a later cell's authority.
     try {
       new AsyncFunction(code);
       body = new AsyncFunction(...parameters,'return (async function() {\n'+code+'\n})();');
     }
     catch(e) { if (!(e instanceof SyntaxError)) throw e; bodyError = e; }
     // Validate the unwrapped source before trying a return wrapper. An extra
     // closing parenthesis must never escape the wrapper and skip later code.
     let sourceValid = !!body;
     if (!sourceValid) {
       try { new AsyncFunction('void '+code); sourceValid = true; }
       catch(e) { if (!(e instanceof SyntaxError)) throw e; }
     }
     if (!sourceValid) throw bodyError;
     let expression = trimEnd(code);
     if (endsWith(expression,';')) expression = slice(expression,0,-1);
     let cell = body;
     try { cell = new AsyncFunction(...parameters,'return (\n'+expression+'\n);'); }
     catch(e) { if (!(e instanceof SyntaxError)) throw e; if (!body) throw bodyError; }
     return cell(state,tools,text,file,image,wait,cmd,console);
   },
   encodeResult(value) {
     if (value === undefined) return '';
     const ancestors = new Set(); let nodes = 0;
     const invalid = detail => { throw new TypeError('JS return value must be JSON-compatible: '+detail); };
     function copy(value, depth) {
       if (++nodes > 100000 || depth > 128) invalid('structure limit exceeded');
       if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
       if (typeof value === 'number') { if (!finite(value)) invalid('non-finite number'); return value; }
       if (typeof value !== 'object') invalid(typeof value);
       if (ancestors.has(value)) invalid('cyclic object');
       const array = isArray(value), prototype = getPrototype(value);
       if (prototype !== (array ? arrayPrototype : objectPrototype) && prototype !== null) invalid('unsupported object type');
       const ds = descriptors(value), names = ownKeys(ds);
       if (names.some(key => typeof key === 'symbol')) invalid('symbol property');
       ancestors.add(value);
       const result = array ? setPrototype([],null) : create(null);
       if (array) {
         const length = ds.length.value;
         if (length > 100000 || names.length !== length + 1) invalid('sparse array or extra array properties');
         for (let i=0; i<length; i++) {
           const d = ds[String(i)];
           if (!d || !('value' in d)) invalid('array accessor or hole');
           result[i] = copy(d.value,depth+1);
         }
       } else {
         for (const key of names) {
           if (value===state && key==='reset') continue;
           const d = ds[key];
           if (!('value' in d) || !d.enumerable) invalid('accessor or non-enumerable property');
           if (d.value === undefined) {
             // Omission still consumes the same traversal/depth budget.
             if (++nodes > 100000 || depth + 1 > 128) invalid('structure limit exceeded');
             continue;
           }
           result[key] = copy(d.value,depth+1);
         }
       }
       ancestors.delete(value);
       return result;
     }
     return stringify(copy(value,0));
   },
   stateNotices,
   resolve(id, ok, encoded) {
     const p = pending.get(id); if (!p) return;
     pending.delete(id);
     if (ok) p.resolve(parse(encoded)); else p.reject(new Error(encoded));
   },
   inspect() {
     const ds = descriptors(state), names = keys(ds).filter(key=>key!=='reset'); let entries = '', bytes = 0;
     for (let i=0; i<names.length && i<64; i++) { const key = names[i];
       const d = ds[key], t = 'value' in d ? typeof d.value : 'accessor';
       let s = '{"key":'+stringify(slice(key,0,128))+',"type":'+stringify(t);
       if (t==='string') s += ',"preview":'+stringify(slice(d.value,0,160));
       else if (t==='number' && finite(d.value)) s += ',"preview":'+stringify(d.value);
       else if (t==='boolean' || (t==='object' && d.value===null)) s += ',"preview":'+stringify(d.value);
       s += '}'; bytes += s.length; if(bytes > 16384) break; entries += (entries ? ',' : '')+s;
     }
     return '['+entries+']';
   }
 };
})"#;

#[derive(Default)]
struct View {
    generation: u64,
    execution_id: Option<String>,
    status: String,
    waiting: usize,
    sync_exec_waiting: usize,
    queued_other_waiting: usize,
    time_waiting: usize,
    handles: Vec<String>,
    state: Value,
    outputs: Vec<Value>,
    output_bytes: usize,
    logs: Vec<String>,
    log_bytes: usize,
    logs_omitted: usize,
    images: Option<Arc<Mutex<crate::images::ImageBatch>>>,
    last_error: Option<String>,
    last_process_diagnostic: Option<String>,
    last_result: Option<Value>,
    result_execution_id: Option<String>,
    state_changes: Vec<String>,
    pending_reset_notice: Option<String>,
    background: bool,
    resetting: bool,
}
impl View {
    fn background_on_steer(&mut self) -> bool {
        let ordinary_pending = if self.sync_exec_waiting > 0 {
            self.waiting.saturating_sub(self.sync_exec_waiting) + self.queued_other_waiting
        } else {
            self.waiting
        };
        if self.execution_id.is_some() && ordinary_pending > 0 && !self.background {
            self.background = true;
            self.status = "background".into();
            true
        } else {
            false
        }
    }
    fn result(&self, name: &str) -> Value {
        let mut result = self.json(name);
        result.as_object_mut().unwrap().remove("state");
        result
    }
    fn json(&self, name: &str) -> Value {
        // Output text is kept whole. The whole-output budget below bounds every
        // path that reaches the model (result, inspection, environment listing
        // and environment actions): later outputs are dropped and counted.
        let mut bytes = 0;
        let outputs = self
            .outputs
            .iter()
            .map(|output| {
                let mut preview = output.clone();
                if let Some(alt) = preview["alt"].as_str() {
                    let truncated = alt.chars().count() > 160;
                    preview["alt"] = json!(alt.chars().take(160).collect::<String>());
                    if truncated {
                        preview["alt_truncated"] = json!(true);
                    }
                }
                preview
            })
            .take_while(|output| {
                bytes += output.to_string().len();
                bytes <= 24 * 1024
            })
            .collect::<Vec<_>>();
        json!({"name":name,"generation":self.generation,"execution_id":self.execution_id,
            "status":self.status,"waiting":self.waiting,"time_waiting":self.time_waiting,"handles":self.handles,
            "state":self.state,"output_count":self.outputs.len(),
            "outputs_omitted":self.outputs.len() - outputs.len(),"outputs":outputs,"last_error":self.last_error,
            "result_available":self.last_result.is_some(),"result_execution_id":self.result_execution_id,
            "state_changes":self.state_changes})
    }
    fn inspection(&self, name: &str) -> Value {
        let mut inspection = self.json(name);
        if let Some(result) = &self.last_result {
            inspection["result"] = result.clone();
            // Keep an intact return value within the dispatcher's 64 KiB reply
            // budget; explicitly mark any omitted environment previews.
            let mut omitted = Vec::new();
            for field in ["outputs", "state", "handles"] {
                if inspection.to_string().len() <= MAX_RETURN_BYTES + 2048 {
                    break;
                }
                inspection.as_object_mut().unwrap().remove(field);
                omitted.push(field);
                inspection["inspection_omitted"] = json!(omitted);
            }
        }
        inspection
    }
}
struct Environment {
    sender: mpsc::Sender<Command>,
    view: Arc<Mutex<View>>,
    cancellation: Arc<Mutex<Option<CancellationToken>>>,
    notify: Arc<Notify>,
    wake: Arc<Notify>,
    wake_epoch: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}
struct CallerGuard {
    environment: Arc<Environment>,
    token: CancellationToken,
    armed: bool,
}
impl Drop for CallerGuard {
    fn drop(&mut self) {
        if self.armed && !self.environment.view.lock().unwrap().background {
            self.token.cancel();
        }
    }
}
enum Command {
    Execute(Box<CellCommand>),
    Reset,
    Close,
}
struct CellCommand {
    context: ToolContext,
    dispatcher: ToolDispatcher,
    code: String,
    name: String,
    handle: tokio::runtime::Handle,
    max_output_tokens: usize,
    images: Arc<Mutex<crate::images::ImageBatch>>,
    done: oneshot::Sender<Result<String>>,
}
struct CellResult {
    value: Option<Value>,
    state_changes: Vec<String>,
}
fn bounded_notices(notices: Vec<String>) -> Vec<String> {
    const OMITTED: &str = "additional state notices omitted (size limit)";
    let mut selected = Vec::new();
    let mut bytes = OMITTED.len() + 1;
    for notice in notices {
        if bytes + notice.len() + 1 > MAX_NOTICE_BYTES {
            selected.push(OMITTED.to_owned());
            break;
        }
        bytes += notice.len() + 1;
        selected.push(notice);
    }
    selected
}
fn with_state_notices(mut content: String, notices: &[String]) -> String {
    if notices.is_empty() {
        content
    } else {
        content.push_str("\n[---]\n");
        content.push_str(&notices.join("\n"));
        content
    }
}
struct HostCall {
    id: String,
    name: String,
    input: Value,
    wait: WaitKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    None,
    Operation,
    Time,
    SyncExec,
}
impl WaitKind {
    fn for_call(name: &str, input: &Value) -> Self {
        match name {
            "wait" if input.get("ms").is_some() => Self::Time,
            "wait" | "web_search" | "web_fetch" | "question" => Self::Operation,
            "poll"
                if input["yieldMs"]
                    .as_u64()
                    .is_some_and(|yield_ms| yield_ms > 0) =>
            {
                Self::Operation
            }
            "exec"
                if input["background"].as_bool() != Some(true)
                    && input["yieldMs"].as_u64() != Some(0) =>
            {
                Self::SyncExec
            }
            _ => Self::None,
        }
    }
    fn is_wait(self) -> bool {
        self != Self::None
    }
    fn enqueue(self, view: &mut View) {
        if matches!(self, Self::Operation | Self::Time) {
            view.queued_other_waiting += 1;
        }
    }
}
struct Completion {
    id: String,
    wait: WaitKind,
    result: Result<Value>,
}

pub struct CodeMode {
    artifacts: Arc<ArtifactStore>,
    output_budget: Arc<OutputBudget>,
    environments: Mutex<BTreeMap<(String, String), Arc<Environment>>>,
    idle_limit: Duration,
}
impl CodeMode {
    pub fn new(root: PathBuf, artifacts: Arc<ArtifactStore>) -> Result<Self> {
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "CodeMode workspace must be a directory");
        let spills = crate::spill::SpillStore::new(artifacts.data_dir())?;
        Ok(Self {
            artifacts,
            output_budget: Arc::new(OutputBudget::new(spills)?),
            environments: Mutex::new(BTreeMap::new()),
            idle_limit: IDLE_LIMIT,
        })
    }
    /// Replace the 30 s idle limit for JS host calls; tests use a short one.
    pub fn with_idle_limit(mut self, limit: Duration) -> Self {
        self.idle_limit = limit;
        self
    }
    pub fn register(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        crate::environments::register(self, registry)?;
        self.register_js(registry)
    }
    /// Register only the JavaScript model entrypoint. Hosts can register
    /// environment controls independently with [`crate::environments::register`].
    pub fn register_js(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        registry.register(Arc::new(JsTool { mode: self.clone() }))
    }
    pub(crate) fn spill_store(&self) -> &crate::spill::SpillStore {
        &self.output_budget.spills
    }
    fn environment(&self, session: &str, name: &str, create: bool) -> Result<Arc<Environment>> {
        ensure!(!session.is_empty(), "A session is required");
        ensure!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid environment name"
        );
        let key = (session.to_owned(), name.to_owned());
        let mut envs = self.environments.lock().unwrap();
        envs.retain(|_, env| {
            !(env.closed.load(Ordering::Acquire)
                && env
                    .join
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|j| j.is_finished()))
        });
        if let Some(env) = envs.get(&key) {
            ensure!(
                !env.closed.load(Ordering::Acquire),
                "Environment is closing"
            );
            return Ok(env.clone());
        }
        ensure!(
            create,
            "Unknown environment: {name}. Create it with environments first"
        );
        ensure!(
            envs.len() < MAX_ENVS,
            "Environment limit reached ({MAX_ENVS} total)"
        );
        ensure!(
            envs.keys().filter(|(s, _)| s == session).count() < MAX_SESSION_ENVS,
            "Session environment limit reached ({MAX_SESSION_ENVS})"
        );
        let view = Arc::new(Mutex::new(View {
            generation: 1,
            status: "idle".into(),
            state: json!([]),
            ..View::default()
        }));
        let cancellation = Arc::new(Mutex::new(None));
        let notify = Arc::new(Notify::new());
        let wake = Arc::new(Notify::new());
        let wake_epoch = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let artifacts = self.artifacts.clone();
        let output_budget = self.output_budget.clone();
        let idle_limit = self.idle_limit;
        let env = Arc::new(Environment {
            sender,
            view: view.clone(),
            cancellation: cancellation.clone(),
            notify: notify.clone(),
            wake: wake.clone(),
            wake_epoch: wake_epoch.clone(),
            closed: closed.clone(),
            join: Mutex::new(None),
        });
        let worker_env = env.clone();
        let handle = thread::Builder::new()
            .name(format!("codemode-{name}"))
            .spawn(move || {
                worker(receiver, worker_env, artifacts, output_budget, idle_limit);
            })
            .context("Starting QuickJS worker")?;
        *env.join.lock().unwrap() = Some(handle);
        envs.insert(key, env.clone());
        Ok(env)
    }
    pub fn status(&self, session: &str) -> Value {
        let mut envs = self.environments.lock().unwrap();
        envs.retain(|_, env| {
            !(env.closed.load(Ordering::Acquire)
                && env
                    .join
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|j| j.is_finished()))
        });
        json!({"environments":envs.iter().filter(|((s,_),_)| s == session).map(|((_,name),env)| env.view.lock().unwrap().json(name)).collect::<Vec<_>>()})
    }
    pub fn control(&self, session: &str, input: Value) -> Result<Value> {
        let action = input["action"].as_str().unwrap_or("inspect");
        let name = input["environment"].as_str().unwrap_or("main");
        if action == "inspect" && input.get("environment").is_none() {
            return Ok(serde_json::from_str(&self.output_budget.limit(
                session,
                &self.status(session).to_string(),
                &[],
                DEFAULT_OUTPUT_TOKENS,
            ))?);
        }
        let env = self.environment(session, name, action == "create")?;
        let mut view = env.view.lock().unwrap();
        match action {
            "create" | "inspect" => {}
            "background" => {
                ensure!(
                    view.execution_id.is_some() && view.waiting > 0,
                    "Only a cell with a pending explicit wait can background"
                );
                view.background = true;
                view.status = "background".into();
                env.notify.notify_one();
            }
            "wake" => {
                ensure!(
                    view.time_waiting > 0,
                    "Wake requires a pending time wait; operation waits cannot be force-completed"
                );
                env.wake_epoch.fetch_add(1, Ordering::AcqRel);
                env.wake.notify_waiters();
                view.background = false;
                view.status = if view.execution_id.is_none() {
                    "idle"
                } else if view.waiting > 0 {
                    "waiting"
                } else {
                    "running"
                }
                .into();
            }
            "cancel" => {
                if let Some(c) = env.cancellation.lock().unwrap().as_ref() {
                    c.cancel();
                }
            }
            "reset" | "close" => {
                ensure!(
                    view.execution_id.is_none() || input["cancel"].as_bool() == Some(true),
                    "Environment is busy; cancel it first or pass cancel:true"
                );
                if let Some(c) = env.cancellation.lock().unwrap().as_ref() {
                    c.cancel();
                }
                if action == "close" {
                    env.closed.store(true, Ordering::Release);
                    view.status = "closing".into();
                } else {
                    view.resetting = true;
                    view.status = "resetting".into();
                }
                env.sender.send(if action == "close" {
                    Command::Close
                } else {
                    Command::Reset
                })?;
            }
            _ => bail!("Unknown environment action: {action}"),
        }
        let result = if action == "inspect" {
            view.inspection(name)
        } else {
            view.json(name)
        };
        drop(view);
        // Nested inspection must itself fit the host dispatcher's byte cap,
        // and cannot re-expose a large eventual background return.
        Ok(serde_json::from_str(&self.output_budget.limit(
            session,
            &result.to_string(),
            &[],
            DEFAULT_OUTPUT_TOKENS,
        ))?)
    }
    pub(crate) fn inspected_images(
        &self,
        session: &str,
        input: &Value,
    ) -> crate::images::ImageBatch {
        let mut images = crate::images::ImageBatch::default();
        if input["action"].as_str().unwrap_or("inspect") != "inspect" {
            return images;
        }
        let selected = input["environment"].as_str();
        for ((owner, name), environment) in self.environments.lock().unwrap().iter() {
            if owner == session && selected.is_none_or(|selected| selected == name) {
                let batch = environment.view.lock().unwrap().images.clone();
                if let Some(batch) = batch {
                    images.merge(&batch.lock().unwrap());
                }
            }
        }
        images
    }
    /// Explicit host wait release. Ordinary accepted steering does not call it.
    /// This does not cancel the continuation or its nested operations.
    pub fn background_waits(&self, session: &str) -> usize {
        let envs = self.environments.lock().unwrap();
        let mut count = 0;
        for ((s, _), env) in envs.iter().filter(|((s, _), _)| s == session) {
            let _ = s;
            let mut view = env.view.lock().unwrap();
            if view.background_on_steer() {
                count += 1;
                env.notify.notify_one();
            }
        }
        count
    }
    pub fn cancel_session(&self, session: &str) -> usize {
        let envs = self.environments.lock().unwrap();
        let mut count = 0;
        for ((s, _), env) in envs.iter() {
            if s == session
                && let Some(c) = env.cancellation.lock().unwrap().as_ref()
            {
                c.cancel();
                count += 1;
            }
        }
        count
    }
    pub async fn shutdown(&self) {
        let envs = std::mem::take(&mut *self.environments.lock().unwrap());
        let mut joins = Vec::new();
        for env in envs.into_values() {
            env.closed.store(true, Ordering::Release);
            if let Some(c) = env.cancellation.lock().unwrap().as_ref() {
                c.cancel();
            }
            let _ = env.sender.send(Command::Close);
            if let Some(j) = env.join.lock().unwrap().take() {
                joins.push(j);
            }
        }
        let _ = tokio::task::spawn_blocking(move || {
            for j in joins {
                let _ = j.join();
            }
        })
        .await;
    }
    async fn execute(
        &self,
        mut context: ToolContext,
        dispatcher: ToolDispatcher,
        input: Value,
        images: Arc<Mutex<crate::images::ImageBatch>>,
    ) -> Result<String> {
        let code = input["code"]
            .as_str()
            .context("js requires code")?
            .to_owned();
        ensure!(code.len() <= MAX_CODE, "Cell source exceeds 128 KiB");
        let max_output_tokens = output_tokens(&input)?;
        let session = context.session_id.clone();
        let name = input["environment"].as_str().unwrap_or("main");
        let env = self.environment(&context.session_id, name, name == "main")?;
        let id = uuid::Uuid::new_v4().to_string();
        let token = CancellationToken::new();
        // The host explicitly cancels environment work. Caller cancellation/drop
        // must not silently destroy a background continuation.
        context.cancellation = token.clone();
        {
            let mut view = env.view.lock().unwrap();
            ensure!(
                view.execution_id.is_none(),
                "Environment already has an active cell"
            );
            ensure!(!view.resetting, "Environment reset is pending");
            ensure!(!env.closed.load(Ordering::Acquire), "Environment closed");
            view.execution_id = Some(id);
            view.status = "running".into();
            view.background = false;
            view.outputs.clear();
            view.output_bytes = 0;
            view.logs.clear();
            view.log_bytes = 0;
            view.logs_omitted = 0;
            view.images = Some(images.clone());
            view.last_error = None;
            view.last_process_diagnostic = None;
            view.last_result = None;
            view.result_execution_id = None;
            view.state_changes.clear();
            view.handles.clear();
            *env.cancellation.lock().unwrap() = Some(token.clone());
        }
        let mut guard = CallerGuard {
            environment: env.clone(),
            token,
            armed: true,
        };
        let (sender, mut receiver) = oneshot::channel();
        env.sender.send(Command::Execute(Box::new(CellCommand {
            context,
            dispatcher,
            code,
            name: name.to_owned(),
            handle: tokio::runtime::Handle::current(),
            max_output_tokens,
            images,
            done: sender,
        })))?;
        loop {
            let notification = env.notify.notified();
            {
                let view = env.view.lock().unwrap();
                if view.background {
                    return Ok(self.output_budget.limit(
                        &session,
                        &view.result(name).to_string(),
                        &[],
                        max_output_tokens,
                    ));
                }
            }
            tokio::select! {
                result = &mut receiver => { guard.armed = false; return result.context("QuickJS worker stopped")?; },
                _ = notification => {},
            }
        }
    }
}
impl Drop for CodeMode {
    fn drop(&mut self) {
        for env in self.environments.get_mut().unwrap().values() {
            env.closed.store(true, Ordering::Release);
            if let Some(c) = env.cancellation.lock().unwrap().as_ref() {
                c.cancel();
            }
            let _ = env.sender.send(Command::Close);
        }
    }
}

/// `tools.$search` and `tools.$list`, over the dispatcher's visible capabilities.
fn discovery(owner: &Owner, call: &HostCall) -> Result<Value> {
    let capabilities = owner.dispatcher.definitions();
    if call.name == "#list" {
        return Ok(Value::Array(
            capabilities
                .iter()
                .map(|d| json!({"name":d.name,"summary":tool_definition_summary(&d.description),"shownAlways":owner.dispatcher.is_shown_always(&d.name)}))
                .collect(),
        ));
    }
    let query = call.input["query"]
        .as_str()
        .context("tools.$search requires a query string")?;
    let limit = match call.input.get("limit") {
        None | Some(Value::Null) => None,
        Some(value) => value
            .as_u64()
            .filter(|n| *n >= 1)
            .context("tools.$search limit must be a positive integer")?
            .min(50)
            .try_into()
            .ok(),
    };
    Ok(search_tool_definitions_with_servers(
        &capabilities,
        query,
        limit,
        |name| owner.dispatcher.tool_server(name),
    ))
}
struct JsTool {
    mode: Arc<CodeMode>,
}
#[async_trait]
impl Tool for JsTool {
    fn interrupt_wait(&self, session: &str) -> usize {
        self.mode.background_waits(session)
    }
    fn cancel_session(&self, session: &str) -> usize {
        self.mode.cancel_session(session)
    }
    fn definition(&self) -> ToolDefinition {
        self.definition_in(&ToolRegistry::new())
    }
    /// Generated per request from the registry's current capabilities, so
    /// tools registered after `js` appear and restrictions are respected.
    fn definition_in(&self, registry: &ToolRegistry) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "js".into(),
            description: js_reference(registry),
            read_only: true,
            input_schema: json!({"type":"object","properties":{"environment":{"type":"string"},"code":{"type":"string"},"maxOutputTokens":{"type":"integer","minimum":256}},"required":["code"],"additionalProperties":false}),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        bail!("js requires the host tool dispatcher")
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.mode.output_budget.spills.attach(dispatcher.store());
        let tokens = output_tokens(&input).unwrap_or(DEFAULT_OUTPUT_TOKENS);
        let session = context.session_id.clone();
        let images = Arc::new(Mutex::new(crate::images::ImageBatch::default()));
        let mut result = match self
            .mode
            .execute(context, dispatcher, input, images.clone())
            .await
        {
            Ok(content) => ToolResult::text(self.mode.output_budget.limit(
                &session,
                &content,
                &[],
                tokens,
            )),
            Err(error) => {
                let error = error.to_string();
                let (content, notices) =
                    if let Some((content, notices)) = error.rsplit_once("\n[---]\n") {
                        (
                            content,
                            notices.lines().map(str::to_owned).collect::<Vec<_>>(),
                        )
                    } else {
                        (error.as_str(), Vec::new())
                    };
                ToolResult::error(
                    self.mode
                        .output_budget
                        .limit(&session, content, &notices, tokens),
                )
            }
        };
        result.content = images.lock().unwrap().attach(result.content);
        Ok(result)
    }
}
struct Engine {
    // Persistent values must drop before their context/runtime.
    api: Persistent<Object<'static>>,
    context: Context,
    runtime: Runtime,
    calls: mpsc::Receiver<HostCall>,
    current: Arc<Mutex<Option<Owner>>>,
    deadline: Arc<Mutex<Option<Instant>>>,
    inflight: Arc<AtomicUsize>,
}
#[derive(Clone)]
struct Owner {
    context: ToolContext,
    dispatcher: ToolDispatcher,
    id: String,
    generation: u64,
    environment: String,
    view: Arc<Mutex<View>>,
    wake: Arc<Notify>,
    wake_epoch: Arc<AtomicUsize>,
    idle_limit: Duration,
    images: Arc<Mutex<crate::images::ImageBatch>>,
}
impl Owner {
    fn process_diagnostic(&self, diagnostic: Option<String>) {
        let mut view = self.view.lock().unwrap();
        if view.execution_id.as_deref() == Some(self.id.as_str()) {
            view.last_process_diagnostic = diagnostic.map(|text| text.chars().take(2048).collect());
        }
    }
    fn output(&self, output: Value) -> Result<()> {
        ensure!(!self.context.cancellation.is_cancelled(), "Cell cancelled");
        let size = output.to_string().len();
        let mut view = self.view.lock().unwrap();
        ensure!(
            view.execution_id.as_deref() == Some(self.id.as_str()),
            "Output belongs to a completed cell"
        );
        if output["type"] == "log" {
            if let Some(line) = output["text"].as_str()
                && view.logs.len() < MAX_LOG_LINES
                && view.log_bytes.saturating_add(line.len()) <= MAX_LOG_BYTES
            {
                view.logs.push(line.to_owned());
                view.log_bytes += line.len();
            } else {
                view.logs_omitted += 1;
            }
            return Ok(());
        }
        ensure!(
            size <= 64 * 1024
                && view.outputs.len() < MAX_OUTPUTS
                && view.output_bytes.saturating_add(size) <= MAX_OUTPUT_BYTES,
            "Cell output limit reached"
        );
        self.dispatcher
            .emit_output(&self.context, &self.id, output.clone())?;
        view.outputs.push(output);
        view.output_bytes += size;
        Ok(())
    }
}
impl Engine {
    fn new() -> Result<Self> {
        let runtime = Runtime::new().map_err(|e| anyhow!(e.to_string()))?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(256 * 1024);
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let current: Arc<Mutex<Option<Owner>>> = Arc::new(Mutex::new(None));
        let d = deadline.clone();
        let owner = current.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            owner
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|o| o.context.cancellation.is_cancelled())
                || d.lock().unwrap().is_some_and(|d| Instant::now() >= d)
        })));
        let context = Context::full(&runtime).map_err(|e| anyhow!(e.to_string()))?;
        let (sender, calls) = mpsc::channel();
        let owner = current.clone();
        let emit_owner = current.clone();
        let inflight = Arc::new(AtomicUsize::new(0));
        let queue_count = inflight.clone();
        let api = context
            .with(|ctx| -> rquickjs::Result<_> {
                let enqueue = Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>,
                          execution: String,
                          id: String,
                          name: String,
                          encoded: String|
                          -> rquickjs::Result<()> {
                        let current = owner.lock().unwrap();
                        if !current.as_ref().is_some_and(|o| {
                            o.id == execution && !o.context.cancellation.is_cancelled()
                        }) {
                            return Err(rquickjs::Exception::throw_message(
                                &ctx,
                                "Cell no longer owns this capability",
                            ));
                        }
                        if encoded.len() > MAX_CODE {
                            return Err(rquickjs::Exception::throw_message(
                                &ctx,
                                "Tool input too large",
                            ));
                        }
                        let input = serde_json::from_str(&encoded).map_err(|_| {
                            rquickjs::Exception::throw_message(&ctx, "Invalid tool input")
                        })?;
                        #[allow(deprecated)]
                        // fetch_update supports the workspace's Rust 1.88 MSRV.
                        let reserved =
                            queue_count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                                (v < MAX_PENDING).then_some(v + 1)
                            });
                        if reserved.is_err() {
                            return Err(rquickjs::Exception::throw_message(
                                &ctx,
                                "Pending host-call limit reached",
                            ));
                        }
                        let wait = WaitKind::for_call(&name, &input);
                        wait.enqueue(&mut current.as_ref().unwrap().view.lock().unwrap());
                        sender
                            .send(HostCall {
                                id,
                                name,
                                input,
                                wait,
                            })
                            .map_err(|_| rquickjs::Exception::throw_message(&ctx, "Worker stopped"))
                    },
                )?;
                let emit = Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>,
                          execution: String,
                          encoded: String|
                          -> rquickjs::Result<()> {
                        if encoded.len() > MAX_OUTPUT_BYTES {
                            return Err(rquickjs::Exception::throw_message(
                                &ctx,
                                "Output too large",
                            ));
                        }
                        let owner = emit_owner.lock().unwrap();
                        let owner =
                            owner
                                .as_ref()
                                .filter(|o| o.id == execution)
                                .ok_or_else(|| {
                                    rquickjs::Exception::throw_message(
                                        &ctx,
                                        "Cell no longer owns output",
                                    )
                                })?;
                        let value = serde_json::from_str(&encoded).map_err(|_| {
                            rquickjs::Exception::throw_message(&ctx, "Invalid output")
                        })?;
                        owner
                            .output(value)
                            .map_err(|e| rquickjs::Exception::throw_message(&ctx, &e.to_string()))
                    },
                )?;
                let factory: Function = ctx.eval(BOOTSTRAP)?;
                let api: Object = factory.call((enqueue, emit))?;
                Ok(Persistent::save(&ctx, api))
            })
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(Self {
            api,
            context,
            runtime,
            calls,
            current,
            deadline,
            inflight,
        })
    }
    fn inspection(&self) -> Result<Value> {
        self.context
            .with(|ctx| {
                let api = self.api.clone().restore(&ctx)?;
                let inspect: Function = api.get("inspect")?;
                inspect.call::<_, String>(())
            })
            .map_err(|e| anyhow!(e.to_string()))
            .and_then(|s| Ok(serde_json::from_str(&s)?))
    }
    fn execute(
        &mut self,
        owner: Owner,
        code: String,
        artifacts: Arc<ArtifactStore>,
        handle: tokio::runtime::Handle,
    ) -> Result<CellResult> {
        *self.current.lock().unwrap() = Some(owner.clone());
        let definitions = owner
            .dispatcher
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>();
        let names = definitions
            .into_iter()
            .filter(|n| n != "wait")
            .collect::<Vec<_>>();
        let names = serde_json::to_string(&names)?;
        let mut budget = JS_BUDGET;
        *self.deadline.lock().unwrap() = Some(Instant::now() + budget);
        let start = Instant::now();
        let promise = self.context.with(|ctx| -> Result<_> {
            let api = self.api.clone().restore(&ctx)?;
            let execute: Function = api.get("execute")?;
            let promise: Promise = execute
                .call((owner.id.clone(), code, names))
                .map_err(|e| js_cell_error(&ctx, e, 2))?;
            Ok(Persistent::save(&ctx, promise))
        })?;
        budget = budget.saturating_sub(start.elapsed());
        *self.deadline.lock().unwrap() = None;
        let (sender, receiver) = mpsc::channel::<Completion>();
        let mut completions = VecDeque::new();
        let mut pending = BTreeMap::<String, (bool, tokio::task::JoinHandle<()>)>::new();
        let mut queued = VecDeque::new();
        let mut idle_since = Instant::now();
        // Time waiting for a person's approval is neither idle nor call time.
        let approval_waits = Arc::new(AtomicUsize::new(0));
        let result = (|| -> Result<CellResult> {
            loop {
                ensure!(!owner.context.cancellation.is_cancelled(), "Cell cancelled");
                ensure!(!budget.is_zero(), "Active JavaScript time limit exceeded");
                queued.extend(self.calls.try_iter());
                while let Some(call) = queued.pop_front() {
                    ensure!(
                        pending.len() < MAX_PENDING,
                        "Pending host-call limit reached"
                    );
                    let kind = call.wait;
                    let is_wait = kind.is_wait();
                    let is_time = kind == WaitKind::Time;
                    let initial_wake_epoch = owner.wake_epoch.load(Ordering::Acquire);
                    {
                        let mut view = owner.view.lock().unwrap();
                        if is_wait {
                            view.waiting += 1;
                            if kind == WaitKind::SyncExec {
                                view.sync_exec_waiting += 1;
                            } else {
                                view.queued_other_waiting =
                                    view.queued_other_waiting.saturating_sub(1);
                            }
                        }
                        if is_time {
                            view.time_waiting += 1;
                        }
                        if let Some(h) = call.input["handle"].as_str()
                            && h.len() <= 256
                            && view.handles.len() < MAX_PENDING
                            && !view.handles.iter().any(|v| v == h)
                        {
                            view.handles.push(h.to_owned());
                        }
                        if !view.background {
                            view.status = if view.waiting > 0 {
                                "waiting"
                            } else {
                                "running"
                            }
                            .into();
                        }
                    }
                    let id = call.id.clone();
                    let job_id = id.clone();
                    let tx = sender.clone();
                    let o = owner.clone();
                    let store = artifacts.clone();
                    let waits = approval_waits.clone();
                    let job = handle.spawn(track_approval_waits(approval_waits.clone(), async move {
                        let result = if is_wait {
                            if is_time {
                                let notified = o.wake.notified(); tokio::pin!(notified); notified.as_mut().enable();
                                if o.wake_epoch.load(Ordering::Acquire) != initial_wake_epoch { Ok(json!({"status":"woken","reason":"user"})) }
                                else { tokio::select! { _ = notified => Ok(json!({"status":"woken","reason":"user"})), result = host_call(&o,call,store) => result } }
                            } else { host_call(&o,call,store).await }
                        } else { idle_limited(host_call(&o,call,store), &waits, o.idle_limit).await };
                        let _ = tx.send(Completion {
                            id: job_id,
                            wait: kind,
                            result,
                        });
                    }));
                    pending.insert(id.clone(), (is_wait, job));
                }
                completions.extend(receiver.try_iter());
                while let Some(done) = completions.pop_front() {
                    idle_since = Instant::now();
                    self.inflight.fetch_sub(1, Ordering::AcqRel);
                    pending.remove(&done.id);
                    let (ok, encoded) = match done.result {
                        Ok(v) => (true, v.to_string()),
                        Err(e) => (false, e.to_string()),
                    };
                    ensure!(
                        encoded.len() <= 2 * 1024 * 1024,
                        "Tool result exceeds 2 MiB; use file output instead"
                    );
                    if done.wait.is_wait() {
                        let mut view = owner.view.lock().unwrap();
                        view.waiting = view.waiting.saturating_sub(1);
                        if done.wait == WaitKind::SyncExec {
                            view.sync_exec_waiting = view.sync_exec_waiting.saturating_sub(1);
                        }
                        if done.wait == WaitKind::Time {
                            view.time_waiting = view.time_waiting.saturating_sub(1);
                        }
                        if !view.background {
                            view.status = if view.waiting == 0 {
                                "running"
                            } else {
                                "waiting"
                            }
                            .into();
                        }
                        idle_since = Instant::now();
                    }
                    *self.deadline.lock().unwrap() = Some(Instant::now() + budget);
                    let start = Instant::now();
                    self.context.with(|ctx| -> Result<()> {
                        let api = self.api.clone().restore(&ctx)?;
                        let resolve: Function = api.get("resolve")?;
                        resolve
                            .call::<_, ()>((done.id, ok, encoded))
                            .map_err(|e| js_error(&ctx, e))
                    })?;
                    budget = budget.saturating_sub(start.elapsed());
                    *self.deadline.lock().unwrap() = None;
                }
                // Job pumping is bounded even while another explicit wait is pending.
                while self.runtime.is_job_pending() {
                    *self.deadline.lock().unwrap() = Some(Instant::now() + budget);
                    let start = Instant::now();
                    let job = self.runtime.execute_pending_job();
                    budget = budget.saturating_sub(start.elapsed());
                    *self.deadline.lock().unwrap() = None;
                    job.map_err(|_| anyhow!("JavaScript job failed or was interrupted"))?;
                    ensure!(!budget.is_zero(), "Active JavaScript time limit exceeded");
                }
                *self.deadline.lock().unwrap() = Some(Instant::now() + budget);
                let start = Instant::now();
                let settled = self.context.with(|ctx| -> Result<bool> {
                    let p = promise.clone().restore(&ctx)?;
                    match p.result::<JsValue>() {
                        Some(Ok(_)) => Ok(true),
                        Some(Err(e)) => Err(js_error(&ctx, e)),
                        None => Ok(false),
                    }
                });
                budget = budget.saturating_sub(start.elapsed());
                *self.deadline.lock().unwrap() = None;
                let settled = settled?;
                queued.extend(self.calls.try_iter());
                if settled && pending.is_empty() && queued.is_empty() {
                    break;
                }
                let waiting = owner.view.lock().unwrap().waiting;
                if waiting > 0 || approval_waits.load(Ordering::Acquire) > 0 {
                    idle_since = Instant::now();
                } else {
                    ensure!(
                        idle_since.elapsed() < owner.idle_limit,
                        "Non-wait cell idle deadline exceeded"
                    );
                }
                if queued.is_empty()
                    && let Ok(done) = receiver.recv_timeout(Duration::from_millis(50))
                {
                    completions.push_back(done);
                }
            }
            *self.deadline.lock().unwrap() = Some(Instant::now() + budget);
            let returned = self.context.with(|ctx| -> Result<Option<Value>> {
                let p = promise.clone().restore(&ctx)?;
                let value = p.result::<JsValue>().context("Cell result is not settled")?
                    .map_err(|error| js_error(&ctx,error))?;
                let api = self.api.clone().restore(&ctx)?;
                let encode: Function = api.get("encodeResult")?;
                let encoded: String = encode.call((value,)).map_err(|error| js_error(&ctx,error))?;
                ensure!(encoded.len() <= MAX_SERIALIZED_RETURN_BYTES, "JS return value exceeds the 8 MiB serialized resource ceiling; save data in a workspace file instead");
                if encoded.is_empty() { Ok(None) } else { Ok(Some(serde_json::from_str(&encoded)?)) }
            });
            let returned = returned?;
            let mut state_changes = self.context.with(|ctx| -> Result<Vec<String>> {
                let api = self.api.clone().restore(&ctx)?;
                let notices: Function = api.get("stateNotices")?;
                let encoded: String = notices.call(()).map_err(|error| js_error(&ctx, error))?;
                Ok(serde_json::from_str(&encoded)?)
            })?;
            if let Some(notice) = owner.view.lock().unwrap().pending_reset_notice.clone() {
                state_changes.insert(0, notice);
            }
            let inspected = self.inspection();
            *self.deadline.lock().unwrap() = None;
            owner.view.lock().unwrap().state = inspected?;
            Ok(CellResult {
                value: returned,
                state_changes: bounded_notices(state_changes),
            })
        })();
        if result.is_err() {
            owner.context.cancellation.cancel();
        }
        for (_, (_, job)) in pending {
            job.abort();
        }
        *self.current.lock().unwrap() = None;
        *self.deadline.lock().unwrap() = None;
        result
    }
}
/// The non-wait host call deadline, paused while the call (or a sibling in
/// the same cell) waits for a person's approval.
async fn idle_limited(
    call: impl std::future::Future<Output = Result<Value>>,
    approval_waits: &AtomicUsize,
    idle_limit: Duration,
) -> Result<Value> {
    tokio::pin!(call);
    let mut since = Instant::now();
    loop {
        let remaining = idle_limit.saturating_sub(since.elapsed());
        tokio::select! {
            result = &mut call => return result,
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(250))) => {
                if approval_waits.load(Ordering::Acquire) > 0 {
                    since = Instant::now();
                } else if since.elapsed() >= idle_limit {
                    bail!("Non-wait host call deadline exceeded");
                }
            }
        }
    }
}
fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> anyhow::Error {
    js_cell_error(ctx, error, 3)
}
fn js_cell_error(ctx: &Ctx<'_>, error: rquickjs::Error, line_offset: u64) -> anyhow::Error {
    if error.is_exception() {
        let thrown = ctx.catch();
        let text = thrown
            .as_object()
            .and_then(|o| o.get::<_, String>("message").ok())
            .or_else(|| thrown.as_string().and_then(|s| s.to_string().ok()))
            .unwrap_or_else(|| "JavaScript exception".into());
        let mut text = text.chars().take(1024).collect::<String>();
        if let Some(stack) = thrown
            .as_object()
            .and_then(|o| o.get::<_, String>("stack").ok())
            && let Some((_, position)) = stack.split_once("<input>:")
        {
            let mut numbers = position.split(|c: char| !c.is_ascii_digit());
            if let Some(line) = numbers.next().and_then(|n| n.parse::<u64>().ok()) {
                let line = line.saturating_sub(line_offset).max(1);
                text.push_str(&format!(" (line {line}"));
                if let Some(column) = numbers.next().and_then(|n| n.parse::<u64>().ok()) {
                    text.push_str(&format!(", column {column}"));
                }
                text.push(')');
            }
        }
        anyhow!(text)
    } else {
        anyhow!(error.to_string())
    }
}
async fn host_call(owner: &Owner, call: HostCall, artifacts: Arc<ArtifactStore>) -> Result<Value> {
    if call.name == "environments"
        && call.input["environment"].as_str().unwrap_or("main") == owner.environment
        && matches!(
            call.input["action"].as_str(),
            Some("cancel" | "reset" | "close")
        )
    {
        bail!(
            "Cannot cancel/reset/close the executing environment; control it from another environment"
        );
    }
    if call.name == "#search" || call.name == "#list" {
        return discovery(owner, &call);
    }
    if call.name == "#file" || call.name == "#image" {
        let path = call.input["path"]
            .as_str()
            .context("file/image requires path")?
            .to_owned();
        let name = call.input["name"].as_str().map(str::to_owned);
        let image = call.name == "#image";
        let session = owner.context.session_id.clone();
        let artifact = tokio::task::spawn_blocking(move || {
            artifacts.capture(&session, &path, name.as_deref(), image)
        })
        .await??;
        let artifact = serde_json::to_value(artifact)?;
        let mut record = json!({"type":if image {"image"} else {"file"},"artifact":artifact});
        if image && let Some(alt) = call.input["alt"].as_str() {
            record["alt"] = json!(alt);
        }
        owner.output(record)?;
        return Ok(artifact);
    }
    let process_call = matches!(call.name.as_str(), "exec" | "poll" | "cancel")
        || (call.name == "wait"
            && call.input["handle"]
                .as_str()
                .is_some_and(|handle| handle.starts_with("process:")));
    let result = owner
        .dispatcher
        .invoke(owner.context.clone(), &call.name, call.input)
        .await;
    let result = match result {
        Ok(result) if !result.is_error => result,
        result => {
            let cause = match result {
                Ok(result) => result.content,
                Err(error) => format!("{error:#}"),
            };
            if process_call {
                owner.process_diagnostic(Some(cause.clone()));
            }
            bail!("{cause}");
        }
    };
    let (text, images) = crate::images::split_refs(&result.content);
    owner.images.lock().unwrap().collect(images);
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    if process_call
        && (value["handle"]
            .as_str()
            .is_some_and(|handle| handle.starts_with("process:")))
    {
        owner.process_diagnostic(value["diagnostic"].as_str().map(str::to_owned));
    }
    if let Some(h) = value["handle"].as_str().filter(|h| h.len() <= 256) {
        let mut view = owner.view.lock().unwrap();
        if !view.handles.iter().any(|v| v == h) && view.handles.len() < MAX_PENDING {
            view.handles.push(h.to_owned());
        }
    }
    Ok(value)
}
fn worker(
    receiver: mpsc::Receiver<Command>,
    env: Arc<Environment>,
    artifacts: Arc<ArtifactStore>,
    output_budget: Arc<OutputBudget>,
    idle_limit: Duration,
) {
    let view = &env.view;
    let cancellation = &env.cancellation;
    let notify = &env.notify;
    let closed = &env.closed;
    let mut engine = Engine::new();
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Close => break,
            Command::Reset => {
                engine = Engine::new();
                let mut v = view.lock().unwrap();
                v.generation += 1;
                v.state = json!([]);
                v.status = "idle".into();
                v.last_error = None;
                v.last_result = None;
                v.images = None;
                v.result_execution_id = None;
                v.state_changes = vec!["state reset (environment reset)".to_owned()];
                v.pending_reset_notice = Some("state reset (environment reset)".to_owned());
                v.resetting = false;
            }
            Command::Execute(command) => {
                let CellCommand {
                    context,
                    dispatcher,
                    code,
                    name,
                    handle,
                    max_output_tokens,
                    images,
                    done,
                } = *command;
                let id = view.lock().unwrap().execution_id.clone().unwrap();
                let generation = view.lock().unwrap().generation;
                let dispatcher = dispatcher.with_execution(&id, &name, generation);
                let owner = Owner {
                    context,
                    dispatcher,
                    id,
                    generation,
                    environment: name.clone(),
                    view: view.clone(),
                    wake: env.wake.clone(),
                    wake_epoch: env.wake_epoch.clone(),
                    idle_limit,
                    images,
                };
                let result = match &mut engine {
                    Ok(e) => e.execute(owner.clone(), code, artifacts.clone(), handle),
                    Err(e) => Err(anyhow!(e.to_string())),
                };
                let mut v = view.lock().unwrap();
                let result = result.map_err(|error| {
                    if let Some(diagnostic) = &v.last_process_diagnostic {
                        let error = error.to_string();
                        if error.contains(diagnostic.as_str()) {
                            anyhow!(error)
                        } else {
                            anyhow!("{error}\nLast native command outcome: {diagnostic}")
                        }
                    } else {
                        error
                    }
                });
                let result = result.map_err(|error| {
                    let notices = ["state reset (failed/cancelled cell)".to_owned()];
                    anyhow!(with_state_notices(error.to_string(), &notices))
                });
                v.execution_id = None;
                v.waiting = 0;
                v.sync_exec_waiting = 0;
                v.queued_other_waiting = 0;
                v.time_waiting = 0;
                v.background = false;
                v.status = if v.resetting { "resetting" } else { "idle" }.into();
                if let Err(e) = &result {
                    v.last_error = Some(e.to_string());
                    v.generation += 1;
                    v.state = json!([]);
                    engine = Engine::new();
                    v.state_changes = vec!["state reset (failed/cancelled cell)".to_owned()];
                    v.pending_reset_notice =
                        Some("state reset (previous failed/cancelled cell)".to_owned());
                } else if let Ok(cell) = &result {
                    v.state_changes = cell.state_changes.clone();
                    v.pending_reset_notice = None;
                    if cell.value.is_some() {
                        v.result_execution_id = Some(owner.id.clone());
                    }
                }
                *cancellation.lock().unwrap() = None;
                let mut summary = v.result(&name);
                summary["execution_id"] = json!(owner.id);
                summary.as_object_mut().unwrap().remove("state_changes");
                let response = result
                    .map(|cell| {
                        let has_return = cell.value.is_some();
                        let content = output_budget.cell_reply(
                            &owner.context.session_id,
                            cell.value.unwrap_or(summary),
                            &cell.state_changes,
                            max_output_tokens,
                            &v,
                        );
                        if has_return {
                            v.last_result =
                                serde_json::from_str(content.split("\n[---]\n").next().unwrap())
                                    .ok();
                        }
                        content
                    })
                    .map_err(|error| {
                        if v.logs.is_empty() && v.logs_omitted == 0 {
                            error
                        } else {
                            let error = error.to_string();
                            let message = error.split("\n[---]\n").next().unwrap();
                            anyhow!(output_budget.cell_reply(
                                &owner.context.session_id,
                                json!({"error":message}),
                                &v.state_changes,
                                max_output_tokens,
                                &v,
                            ))
                        }
                    });
                let generation = v.generation;
                let error = v.last_error.clone();
                drop(v);
                let mut completion_context = owner.context.clone();
                completion_context.cancellation = CancellationToken::new();
                let completion = owner.dispatcher.emit_output(&completion_context, &owner.id, json!({"type":"execution","environment":name,"generation":owner.generation,"next_generation":generation,"status":if response.is_ok(){"completed"}else{"failed"},"error":error}));
                if let Err(error) = completion {
                    view.lock().unwrap().last_error =
                        Some(format!("Could not persist execution completion: {error}"));
                }
                let _ = done.send(response);
                notify.notify_one();
                if closed.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "codemode_exec_tests.rs"]
mod exec_tests;

#[cfg(test)]
mod js_reference_tests {
    use super::*;
    use crate::{config::ExecConfig, register_native_tools_with_exec};

    struct ReadDescription(&'static str);
    #[async_trait]
    impl Tool for ReadDescription {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "read".into(),
                description: self.0.into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                read_only: true,
                approval: Default::default(),
            }
        }
        async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
            unreachable!("description fixture is never invoked")
        }
    }

    #[test]
    fn js_help_uses_current_native_description_without_changing_it() {
        for text in [
            "Read the authoritative native fixture. Extra native details remain intact.",
            "Load the updated native fixture. Extra native details remain intact.",
        ] {
            let mut registry = ToolRegistry::new();
            registry
                .register_host(Arc::new(ReadDescription(text)))
                .unwrap();
            registry.show_always(["read"]);
            let help = js_reference(&registry);
            assert!(
                help.contains(&format!(
                    "tools.read({{path}}) -> JSON: {}",
                    tool_definition_summary(text)
                )),
                "{help}"
            );
            assert!(!help.contains("Extra native details"));
            let native = registry
                .with_execution_mode(bubblepaw_core::ExecutionMode::Native)
                .definitions();
            assert_eq!(
                native
                    .iter()
                    .find(|tool| tool.name == "read")
                    .unwrap()
                    .description,
                text
            );
        }
    }

    #[test]
    fn js_reference_stays_under_3500_bytes_with_default_registry() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        register_native_tools_with_exec(
            &mut registry,
            root.path(),
            false,
            ExecConfig::default(),
            root.path().join("data"),
        )
        .unwrap();
        let description = js_reference(&registry);
        assert!(
            description.len() < 3500,
            "js description is {} bytes",
            description.len()
        );
        assert!(description.contains("Examples:"));
        assert!(description.contains("Limits per cell: 3 s of active JS"));
        assert!(!description.contains("apply_patch"));
    }
}
