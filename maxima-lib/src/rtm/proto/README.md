# RTM Protocol
These are scraped from the `igoMain-settings-social` webpack chunk of EA Desktop's frontend using Qt's remote debugger.

A useful way of testing these protos and figuring out new ones is passing a captured message into protoc's `--decode_raw` mode.

EADP stands for EA Digital Platform.

## "Antelope"
Upstream's PR 70 renames this protocol `antelope.rtm`. It is the same service
(`rtm.tnt-ea.com:9000`, same length-prefixed framing, same field numbers; its
`Communication { oneof body { CommunicationV1 v1 = 1; } }` is wire-identical to
the plain `v1 = 1` field here, and `optional string` vs `string` only changes
presence tracking), with many more message types for chat, world chat and
moderation. The only behavioural differences are in what clients *send*; they
are available as the `antelope` presence backend
(`MAXIMA_PRESENCE_BACKEND=antelope`, see `src/presence/`).

## Presence is not only RTM
Upstream's PR 67 claims EA app moved presence to a gRPC "social" service
(`api.k.social.ea.com`, protos in `src/presence/proto/`) and keeps RTM only for
the services still on it. That is a different endpoint and protocol, not a
schema of this one; see the `grpc` presence backend.
