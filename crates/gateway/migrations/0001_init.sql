-- Client applications allowed to use the API.
CREATE TABLE apps (
    id               uuid PRIMARY KEY,
    name             text NOT NULL UNIQUE,
    -- Public, non-secret part of the API key, used for lookup.
    key_prefix       text NOT NULL UNIQUE,
    -- SHA-256 of the full API key.
    key_hash         bytea NOT NULL,
    admin            boolean NOT NULL DEFAULT false,
    enabled          boolean NOT NULL DEFAULT true,
    webhook_url      text,
    webhook_secret   text,
    -- Upper-case keywords routing inbound SMS to this app ("RDV ...").
    inbound_keywords text[] NOT NULL DEFAULT '{}',
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messages (
    id                 uuid PRIMARY KEY,
    -- Sending app (outbound) or routed app (inbound, NULL = store only).
    app_id             uuid REFERENCES apps (id) ON DELETE SET NULL,
    direction          text NOT NULL CHECK (direction IN ('outbound', 'inbound')),
    status             text NOT NULL CHECK (status IN
                         ('queued', 'sending', 'sent', 'submitted', 'failed', 'unknown', 'received')),
    -- Recipient (outbound) or sender (inbound), E.164 when possible.
    peer               text NOT NULL,
    -- NULL once purged by the retention job.
    body               text,
    body_sha256        bytea NOT NULL,
    client_ref         text,
    idempotency_key    text,
    -- SHA-256 of the request payload, to detect idempotency key reuse.
    request_hash       bytea,
    attempts           integer NOT NULL DEFAULT 0,
    last_error         text,
    next_attempt_at    timestamptz,
    -- Inbound: reception time as reported by the device (local, naive) and
    -- converted to UTC.
    device_received_at timestamp,
    received_at        timestamptz,
    -- Inbound: how the message was routed (keyword, reply, default, none).
    routed_by          text,
    sent_at            timestamptz,
    body_purged_at     timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (direction = 'inbound' OR status <> 'received'),
    CHECK (direction = 'outbound' OR status = 'received')
);

CREATE UNIQUE INDEX messages_idempotency_key
    ON messages (app_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

-- Inbound dedup: the device reuses inbox indexes after deletion, so the
-- natural key is (sender, device reception time, body hash).
CREATE UNIQUE INDEX messages_inbound_dedup
    ON messages (peer, device_received_at, body_sha256) WHERE direction = 'inbound';

CREATE INDEX messages_outbound_queue
    ON messages (next_attempt_at, created_at) WHERE status = 'queued';
CREATE INDEX messages_sending ON messages (updated_at) WHERE status = 'sending';
CREATE INDEX messages_app_listing ON messages (app_id, created_at, id);
CREATE INDEX messages_reply_correlation
    ON messages (peer, created_at) WHERE direction = 'outbound';
CREATE INDEX messages_created_at ON messages (created_at);

-- Webhook events; the id is the event id sent to receivers.
CREATE TABLE webhook_deliveries (
    id               uuid PRIMARY KEY,
    app_id           uuid NOT NULL REFERENCES apps (id) ON DELETE CASCADE,
    message_id       uuid REFERENCES messages (id) ON DELETE SET NULL,
    event_type       text NOT NULL,
    payload          jsonb NOT NULL,
    status           text NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts         integer NOT NULL DEFAULT 0,
    next_attempt_at  timestamptz NOT NULL DEFAULT now(),
    last_status_code integer,
    last_error       text,
    delivered_at     timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX webhook_deliveries_pending
    ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
CREATE INDEX webhook_deliveries_created_at ON webhook_deliveries (created_at);

-- One row per HTTP attempt.
CREATE TABLE webhook_attempts (
    id           bigserial PRIMARY KEY,
    delivery_id  uuid NOT NULL REFERENCES webhook_deliveries (id) ON DELETE CASCADE,
    attempted_at timestamptz NOT NULL DEFAULT now(),
    status_code  integer,
    error        text,
    duration_ms  integer NOT NULL
);

CREATE INDEX webhook_attempts_delivery ON webhook_attempts (delivery_id);

-- Small persistent key/value state (modem pause...).
CREATE TABLE gateway_settings (
    key        text PRIMARY KEY,
    value      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
