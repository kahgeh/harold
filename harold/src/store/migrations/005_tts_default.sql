-- The default TTS model and voice: one row.
CREATE TABLE tts_default (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    model TEXT NOT NULL,
    voice TEXT NOT NULL
);
