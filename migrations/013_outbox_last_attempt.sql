-- Track the most recent retry attempt so the 72h abandonment window is
-- measured from the first real attempt, not from enqueue time. A large
-- backlog during a long Telegram outage must not abandon rows that were
-- never given a chance to retry.
ALTER TABLE outbox ADD COLUMN last_attempt_at INTEGER;
