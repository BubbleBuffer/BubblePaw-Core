-- Exact v1 catalog SQL: the release's catalog schema and quota history before the
-- balance column, at user_version 1.
PRAGMA application_id=1112556081;
PRAGMA user_version=1;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY,parent_id TEXT,root_id TEXT NOT NULL,title TEXT NOT NULL,last_activity TEXT NOT NULL,created_at TEXT NOT NULL,file TEXT NOT NULL,storage_state TEXT NOT NULL DEFAULT 'active',data TEXT NOT NULL);
CREATE INDEX sessions_root ON sessions(root_id);
CREATE TABLE quota_history(id INTEGER PRIMARY KEY AUTOINCREMENT,provider TEXT NOT NULL,account TEXT NOT NULL,window TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX quota_history_key ON quota_history(account,window,id);
