-- Record the first real retry attempt so the 72h abandonment window starts
-- only after the row has actually been given a chance. Rows that never get a
-- turn during a long Telegram outage are not abandoned by enqueue age.
ALTER TABLE outbox ADD COLUMN first_attempt_at INTEGER;

-- Legacy rows were attempted before this column existed. Use enqueue time as
-- the best available anchor so their next retry cannot reset the 72h window.
UPDATE outbox SET first_attempt_at = created_at WHERE attempts > 0;
