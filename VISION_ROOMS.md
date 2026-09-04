# Buzz Rooms — One Conversation, Many Views

**Status: product direction; not a shipped protocol.** This document defines the
shared conceptual contract for desktop, web, mobile, CLI, and agent integrations.
The backend shape below is a proposal to review before implementation, not an API
clients may assume exists. [VISION.md](VISION.md) covers the platform;
[VISION_PROJECTS.md](VISION_PROJECTS.md) covers its software-forge workflows.

## The Model

A **room is a channel**: one stable channel UUID and one working conversation.
Projects, tasks (including subtasks), repositories, branches, and living documents
each resolve to a canonical room. They are **facets** of that room, not mutually
exclusive room types. An ordinary conversation can have no object facets.

One object has one canonical conversation home within its community. One room can
be home to several objects. A task and its implementation branch can therefore
be two views of **the same channel UUID**, not separate channels with synchronized
messages. Switching from task details to the diff changes the view, not the
conversation, membership, or history. Adding a facet does not create another chat.

Object state stays in its own representation: a task still has status and
assignees, a repository still has refs, and a document still has content. The room
connects those representations; it does not turn every object into a chat message
or replace NIP-34 records with channel rows.

### Identity Is Not a Label

Resolve objects and rooms by stable identity, scoped to the active community:

| Object | Identity to resolve, not a display-name heuristic |
|---|---|
| Room | Channel UUID |
| Project | Address `30621:<owner-pubkey-hex>:<project-id>` |
| Repository | Address `30617:<owner-pubkey-hex>:<repo-id>` |
| Task / subtask | Issue event ID (`kind:1621` today) |
| Branch | Repository address **and full ref**, e.g. `refs/heads/fix-login` |
| Initial document view | The channel's canvas, identified by its channel UUID |

A room rename does not change its identity. The same branch name in two repos is
two different objects. Newer repository/project metadata events replace state at
the same address; their event IDs are not new object identities. A branch rename
or deletion/recreation needs an explicit binding/lifecycle policy, not a guess
based on a matching label. Independently addressable documents beyond canvases
need their own agreed identity contract before implementation.

## Nesting Without Losing Relationships

Rooms can nest. A single parent gives navigation a tree (or a forest of root
rooms); the full object relationships remain a graph. For example:

```text
Project room P                        [project + default repository]
├── Task room T                       [task: Improve sign-in]
│   └── Subtask room S                [task: Fix login + branch: refs/heads/fix-login]
└── Design room D                     [document: sign-in design canvas]
```

The subtask and branch both open room S. S remains nested under T, while its branch
still belongs to the repository in P. A repository view may also link to S; that
is another route to the same room, not a second parent or another conversation.
A dedicated repository room is also valid; sharing the default repository with
the project home is not mandatory for every repository.

Project membership still comes from the project's repository references
([NIP-MP](docs/nips/NIP-MP.md)); nesting rooms does not redefine it. A repository
can belong to several projects and still have one canonical room. Room nesting
also does not by itself define task dependencies or a claimable-work queue.
When several task facets share a room, room containment alone cannot identify
which task is another task's parent; that needs an explicit object relationship.

### Three Separate Decisions

1. **Containment/navigation:** where a room appears and how users find it.
2. **Context:** which parent guidance or related documents an agent should read.
3. **Access:** who may read, write, administer, or push.

A parent link or object association does not grant access, copy membership, or
inherit permissions. Context inheritance is a separate, explicit, permission-
checked policy; a link is not permission to read everything behind it. Navigation
must not expose inaccessible parent/child metadata. An unreadable or unresolved
room does not justify inventing a replacement room or hiding an otherwise
readable object.

**Existing repository bindings are security-sensitive:** `buzz-channel` on a
repository announcement participates in hosted Git access control. The same tag
on a project is navigation metadata, not authority over its repositories. New
room associations must neither overwrite the repo's ACL binding as a navigation
convenience nor become a second, competing source of Git permissions. Linking a
community-visible issue or project to a private room does not make it private.

## What Exists Today

This is the implementation baseline inspected at `2ac0aa1dd`, not a claim that
all of the target model has shipped:

| Existing behavior | Source |
|---|---|
| Desktop project creation provisions a home channel and default repo sharing that home | [createProject.ts](desktop/src/features/projects/createProject.ts) |
| Projects and repos are signed addressable events; tasks are issue events, not dedicated task rows | [NIP-MP](docs/nips/NIP-MP.md), [SDK builders](crates/buzz-sdk/src/builders.rs), [events schema](migrations/0001_initial_schema.sql) |
| Channels have persisted rows; discovery metadata is reconstructed from supported fields | [channel store](crates/buzz-db/src/store/channel.rs), [side effects](crates/buzz-relay/src/handlers/side_effects.rs) |
| Hosted Git uses the repo's channel binding for access, and object storage for Git data | [binding.rs](crates/buzz-relay/src/api/git/binding.rs), [store.rs](crates/buzz-relay/src/api/git/store.rs) |
| Task creation does not provision a task room; issue/PR comments are separate `kind:1` records | [issueMutations.ts](desktop/src/features/projects/issueMutations.ts), [project hooks](desktop/src/features/projects/hooks.ts) |
| Project/issue/Git records remain community-scoped even when carrying an `h` provenance tag | [ingest.rs](crates/buzz-relay/src/handlers/ingest.rs), [CLI command guidance](crates/buzz-cli/src/commands/mod.rs) |

The channel store does not yet provide general room hierarchy or multi-object
bindings. Branch ref state is not a branch-room registry. Threads, huddle parent
links, sidebar sections, and lists of related channels are not substitutes for
shared room containment. The agent harness's [current-room canvas context](crates/buzz-acp/src/queue.rs)
does not implement inherited parent context.

## Smallest Proposed Backend Extension

Keep the channel UUID, conversation, membership, and object event models. Extend
persisted channel metadata with:

- An optional `parent_channel_id` in the same community, indexed for child lookup.
- An `object_bindings` list of typed stable references; JSONB is sufficient for an
  initial representation. Several bindings may point into one room.

Illustrative binding values are a task issue ID, a repository/project address,
or a repository address plus full branch ref. These are **conceptual field
names**, not finalized database columns, tag spellings, or SDK types.

Use the existing signed channel create (`9007`), edit (`9002`), and discovery
metadata (`39000`) path. No feature-specific HTTP endpoint is needed. Proposed
edit semantics are explicit set/clear of the parent and replacement of the
binding list, with unrelated metadata preserved. The implementation must specify
omission, clearing, concurrent edits, and older-client round trips before rollout.
Arbitrary client-added tags do not persist merely because a create/edit event
accepts them: storage and discovery serialization must both support the fields.

The relay must enforce channel-management authority, bounded/valid binding
shapes, an existing same-community parent, no self-parenting or cycles (including
concurrent moves), and atomic metadata updates. It must enforce access on
relationship discovery as well as writes. Cross-object authority needs an
explicit resolution rule: managing a room is not authority to hijack somebody
else's object's canonical home. Existing owner-signed project/repo bindings must
be reconciled, not silently overruled by room-side claims.

A global unique-binding constraint is not required for the first metadata
increment. **Canonicality is still the product contract**, not permission for
each client to choose its own winner. Before enabling canonical-room creation,
define one shared resolution/reconciliation rule for conflicting claims and
idempotent retries. Surface unresolved conflicts instead of choosing whichever
room a client happened to discover first. Clients may create/reuse rooms on
object creation or discovery, but must recover from partial object/room creation
without silently provisioning duplicates. Metadata alone does not solve races.

## Rollout Boundaries

- **One conversation needs a transition.** Issue/PR comments currently have a
  separate event stream; the forge vision also discusses NIP-22 comments. A
  follow-up must define compatible reads, one authoritative write destination,
  provenance, deduplication, and access behavior before claiming a unified room
  conversation. Do not start dual-writing copies of messages or discard history.
- **Automation is separate.** Client-on-discovery provisioning can come first.
  Creating a room for a Git push while every client is offline needs a persistent
  hook/worker. Document which guarantee an implementation actually provides.
- **Lifecycle follows all facets.** A branch merge may archive a branch-only
  room; it must not automatically archive an active task or project sharing that
  room. Unlinking/deleting an object must not erase shared conversation history
  or cascade-delete other facets. Shared-room archive policy needs explicit UX.
- **Private objects and inheritance are not included.** Private task records,
  inherited access, inherited harness context, workflow inheritance, standalone
  document objects, and richer relationship/dependency graphs need separate
  designs. Do not infer them from the proposed metadata fields.

## Implementation Review Checklist

- Do desktop, web, mobile, CLI, and agents resolve the same object to the same
  channel UUID, including retries, conflicts, and community switching?
- Can a task gain a branch view without replacing its identity or conversation?
- Do both task nesting and branch-to-repository relationships survive navigation?
- Are unsupported/unresolved bindings handled without guessed names, duplicate
  rooms, unauthorized disclosure, or a second conversation stream?
- Are membership, object ownership, repository ACLs, and context policies unchanged
  unless the change explicitly designs and tests a new authorization contract?
- Do persistence, edits, discovery, SDKs, and clients round-trip the same metadata?

Related requests: [channel hierarchy #3667](https://github.com/block/buzz/issues/3667)
and [issue relationships #3728](https://github.com/block/buzz/issues/3728). This
vision establishes direction; it does not close either implementation request.
