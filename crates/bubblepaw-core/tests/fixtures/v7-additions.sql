-- Frozen v7 additions from the pre-usage canonical migration.
CREATE TABLE approvals(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),run_id TEXT NOT NULL,call_id TEXT NOT NULL,tool TEXT NOT NULL,arguments TEXT NOT NULL,state TEXT NOT NULL,reason TEXT,created_at TEXT NOT NULL,decided_at TEXT);
CREATE INDEX approvals_session ON approvals(session_id,state);
CREATE TABLE approval_rules(session_id TEXT NOT NULL REFERENCES sessions(id),tool TEXT NOT NULL,PRIMARY KEY(session_id,tool));
ALTER TABLE messages ADD COLUMN message_id TEXT;
UPDATE messages SET message_id=json_extract(data,'$.id');
CREATE INDEX messages_id ON messages(session_id,message_id);
CREATE TABLE blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
CREATE TABLE attachments(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,metadata TEXT NOT NULL,blob_hash TEXT NOT NULL REFERENCES blobs(hash),PRIMARY KEY(session_id,id));
ALTER TABLE inputs ADD COLUMN message_id TEXT;
ALTER TABLE inputs ADD COLUMN content_blob TEXT;
ALTER TABLE inputs ADD COLUMN content_format TEXT;
ALTER TABLE runs ADD COLUMN result_message_id TEXT;
ALTER TABLE approvals ADD COLUMN arguments_ref TEXT;
CREATE TABLE context_checkpoint_versions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),summary_hash TEXT NOT NULL REFERENCES blobs(hash));
PRAGMA user_version=7;
