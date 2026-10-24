-- Frozen per-open tables exactly as the release left them: the tool-call index has
-- no kind column and checkpoint_records.preimage_hash is NOT NULL. message_headers
-- is created on the first open of a record that lacks it, so it is not stored here.
-- The storage_layout marker puts this record on the split-record open path.
INSERT OR REPLACE INTO metadata VALUES('storage_layout','chat-v1');
DROP TABLE IF EXISTS context_views;
CREATE TABLE message_tool_calls(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,call_id TEXT NOT NULL,message_seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,PRIMARY KEY(session_id,call_id,message_seq));
CREATE INDEX message_tool_calls_sequence ON message_tool_calls(session_id,message_seq);
CREATE TABLE checkpoint_blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
CREATE TABLE checkpoint_records(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,boundary_seq INTEGER NOT NULL,metadata TEXT NOT NULL,preimage_hash TEXT NOT NULL REFERENCES checkpoint_blobs(hash),PRIMARY KEY(session_id,id));
CREATE INDEX checkpoint_records_preimage ON checkpoint_records(preimage_hash);
CREATE INDEX checkpoint_records_boundary ON checkpoint_records(session_id,boundary_seq);
