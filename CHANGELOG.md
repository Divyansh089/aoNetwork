# Changelog

## Unreleased

- Keepalive: a ready link pings after `keepalive_interval` with nothing
  received and closes with `KeepaliveTimeout` after `max_missed_pongs`
  unanswered pings; it answers every `Ping` with `Pong`.
- Add `LinkMetrics` (RSSI and RTT moving averages, tx/rx bytes, dropped
  frames, last receive time), read with `Link::metrics` and the new
  `LinkInfo::metrics` field.
- Add `Network::disconnect(link, reason)` for caller-requested link closure and
  connection cancellation through the existing link state machine.
