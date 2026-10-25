-- No question is deferred any more: each one waits again, after the waiting ones,
-- in its present order. Its picks stay.
UPDATE questions
SET state = 'waiting',
    position = position + (SELECT COALESCE(MAX(position), 0) FROM questions WHERE state != 'deferred')
WHERE state = 'deferred';
