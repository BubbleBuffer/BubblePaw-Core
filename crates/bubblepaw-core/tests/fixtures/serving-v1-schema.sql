-- Exact v1 serving projection: the release's serving schema and first request log,
-- before the columns added later, at user_version 1.
PRAGMA application_id=1112556082;
PRAGMA user_version=1;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE context_views(session_id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,status TEXT NOT NULL,data TEXT NOT NULL,result_message_id TEXT);
CREATE TABLE inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,origin TEXT,message_id TEXT,content_blob TEXT,content_format TEXT,resolved_at TEXT NOT NULL,source_seq INTEGER NOT NULL);
CREATE TABLE approvals(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,tool TEXT NOT NULL,arguments TEXT NOT NULL,state TEXT NOT NULL,reason TEXT,created_at TEXT NOT NULL,decided_at TEXT,arguments_ref TEXT);
CREATE TABLE telemetry(id INTEGER PRIMARY KEY,session_id TEXT,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX telemetry_created ON telemetry(created_at);
CREATE INDEX telemetry_record ON telemetry(session_id,json_extract(data,'$.record_id'));
CREATE TABLE events(seq INTEGER PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX events_session ON events(session_id,seq);
CREATE TABLE model_requests(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,lineage TEXT NOT NULL,seq INTEGER NOT NULL,created_at TEXT NOT NULL,frozen_sha256 TEXT NOT NULL,frozen_bytes INTEGER NOT NULL,cold_sha256 TEXT NOT NULL,cold_bytes INTEGER NOT NULL,cold_count INTEGER NOT NULL,history_sha256 TEXT NOT NULL,history_bytes INTEGER NOT NULL,history_count INTEGER NOT NULL,classification TEXT NOT NULL,declared INTEGER NOT NULL,declaration_reason TEXT,provider TEXT NOT NULL,model TEXT NOT NULL,cached_tokens INTEGER,uncached_tokens INTEGER,expected_cached_tokens INTEGER,since_previous_ms INTEGER,warned INTEGER NOT NULL);
CREATE TABLE prompt_tiers(sha256 TEXT PRIMARY KEY,content TEXT NOT NULL) WITHOUT ROWID;
