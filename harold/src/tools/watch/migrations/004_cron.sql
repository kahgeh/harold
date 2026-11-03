-- A watch or a reminder may run on a cron expression.
-- `checked_local` is the newest local minute ("YYYY-MM-DD HH:MM") the schedule checked.
-- Both columns stay NULL for a row that does not use cron.
ALTER TABLE watches ADD COLUMN cron TEXT;
ALTER TABLE watches ADD COLUMN checked_local TEXT;
ALTER TABLE reminders ADD COLUMN cron TEXT;
ALTER TABLE reminders ADD COLUMN checked_local TEXT;
