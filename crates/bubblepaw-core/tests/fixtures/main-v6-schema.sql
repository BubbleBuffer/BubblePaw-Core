-- Exact initialization SQL from 9743334:crates/bubblepaw-core/src/store.rs.
PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=256;

PRAGMA application_id=1112556080; PRAGMA user_version=1;
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS messages(seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id,seq);
          CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),status TEXT NOT NULL,data TEXT NOT NULL);
          CREATE UNIQUE INDEX IF NOT EXISTS one_running ON runs(session_id) WHERE status='running';
          CREATE TABLE IF NOT EXISTS inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS inputs_pending ON inputs(session_id,state,seq);
          CREATE TABLE IF NOT EXISTS effects(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,state TEXT NOT NULL);

ALTER TABLE effects ADD COLUMN detail TEXT; CREATE TABLE outputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),execution_id TEXT NOT NULL,data TEXT NOT NULL); CREATE INDEX outputs_session ON outputs(session_id,seq); CREATE INDEX outputs_execution ON outputs(execution_id); PRAGMA user_version=2;

CREATE TABLE context_checkpoints(session_id TEXT PRIMARY KEY REFERENCES sessions(id),id TEXT UNIQUE NOT NULL,through_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL); PRAGMA user_version=3;

CREATE TABLE context_chapters(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT PRIMARY KEY,ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq)); PRAGMA user_version=4;

ALTER TABLE inputs ADD COLUMN origin TEXT;
            CREATE TABLE context_subject_chapters(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_episode_ordinal INTEGER NOT NULL,last_episode_ordinal INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN ('open','closed')),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_episode_ordinal>0 AND first_episode_ordinal<=last_episode_ordinal));
            CREATE UNIQUE INDEX one_open_subject_chapter ON context_subject_chapters(session_id) WHERE status='open';
            CREATE TABLE context_episodes(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),chapter_id TEXT NOT NULL REFERENCES context_subject_chapters(id),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq));
            CREATE INDEX episodes_chapter ON context_episodes(chapter_id,ordinal);
            CREATE TABLE context_decisions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),episode_ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind='split'),data TEXT NOT NULL,FOREIGN KEY(session_id,episode_ordinal) REFERENCES context_episodes(session_id,ordinal));
            CREATE INDEX decisions_session ON context_decisions(session_id,episode_ordinal);
            PRAGMA user_version=5;

CREATE TABLE context_views(session_id TEXT PRIMARY KEY REFERENCES sessions(id),data TEXT NOT NULL); PRAGMA user_version=6;
