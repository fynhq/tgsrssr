-- Drop the redundant single-column index on subscriptions(chat_id).
-- UNIQUE(chat_id, feed_url) already creates an index whose leftmost prefix
-- covers every chat_id-only lookup (get_subscriptions / count queries), so
-- maintaining this extra index only adds write cost. The consolidated
-- baseline (001_init.sql) kept it by oversight; this is append-only.
DROP INDEX IF EXISTS idx_subscriptions_chat;
