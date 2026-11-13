-- Frozen ticket-67 conversation schema for the version-1 migration fixture.
PRAGMA foreign_keys=ON;
CREATE TABLE sessions (
    id TEXT PRIMARY KEY, conversation TEXT NOT NULL, generation TEXT NOT NULL,
    UNIQUE(conversation,generation));
CREATE TABLE turns (
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
    conversation TEXT NOT NULL, profile TEXT NOT NULL, state TEXT NOT NULL,
    begun_ms INTEGER NOT NULL, ended_ms INTEGER, result TEXT NOT NULL DEFAULT '');
CREATE UNIQUE INDEX one_active_turn ON turns(conversation) WHERE state='running';
CREATE TABLE model_attempts (
    id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id),
    model TEXT NOT NULL, endpoint TEXT NOT NULL, state TEXT NOT NULL,
    begun_ms INTEGER NOT NULL, ended_ms INTEGER, response TEXT);
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
    turn_id TEXT NOT NULL REFERENCES turns(id), attempt_id TEXT REFERENCES model_attempts(id),
    kind TEXT NOT NULL, payload TEXT NOT NULL, created_ms INTEGER NOT NULL);
CREATE INDEX session_events ON events(session_id,seq);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT,'events are append only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT,'events are append only'); END;
PRAGMA user_version=1;
