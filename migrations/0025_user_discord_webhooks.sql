-- Self-service Discord notifications. The instance-wide DISCORD_WEBHOOK_URL
-- (an operator secret, from the environment) stays as it is -- an optional
-- ops channel that hears about every alert. This is a different thing: a
-- person's own webhook, for the items they themselves follow.
--
-- NULL means "not configured", the same way `admin_bootstrap.user_id` uses
-- NULL for "nothing here". It is not exposed on the `User` view type that
-- pages render, the same discipline that already keeps `password_hash` off
-- of it -- a webhook URL is a credential, not a profile fact.
ALTER TABLE users ADD COLUMN discord_webhook_url TEXT;

-- `idx_user_watches_user` answers "what does this person follow". Raising an
-- alert needs the other direction -- "who follows this market" -- and
-- without a leading (item_id, region) index that is a full table scan on
-- every alert.
CREATE INDEX idx_user_watches_item ON user_watches(item_id, region, user_id);
