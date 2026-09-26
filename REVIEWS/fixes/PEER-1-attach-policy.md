# PEER-1: Attach mode, the alpha serves its own settings only

**Findings:** the root half of H-18 (KIMI ABN-H4); M-11 on the attach path.
**Status:** deferred, not in v1. Peering is dangerously experimental and the risk is documented in `docs/peering.md`.

## Problem

While a beta leads, the alpha runs `ssh <leader> autobahn peering attach`, and `attach_as_agent` (`src/transport/mod.rs:965`) runs the full generic `serve_agent` loop for the leader. Every `Initialize` field comes from the leader: root, ignores, symlink mode, file and directory modes, owners, and staging placement. The leader never had any access to the alpha, but it can choose any directory there and any settings. For example, it can send an empty ignore list so the alpha serves files it deliberately ignores. It can also open any number of channels.

## Proposed resolution

- `attach_as_agent` builds an attach policy from the alpha's own configuration. The policy maps each peering group's session identifier to that group's endpoint options.
- `serve_agent` takes an optional policy. Under a policy, `create_endpoint` uses the leader's `Initialize` only to select the group by `session`. It builds the endpoint entirely from the policy's settings and ignores the leader's values.
- An unknown session is refused.
- The number of channels per connection is capped at the number of peering groups.

## Tests

- An attached `Initialize` with `root = "/"` still serves the configured alpha root.
- An empty ignore list from the leader does not expose an ignored file.
- An unknown session is refused.
- Opening more channels than there are groups is refused.
