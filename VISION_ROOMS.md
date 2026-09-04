# Buzz Rooms — One Conversation, Many Views

## The Model

A **room is a channel**: one stable channel UUID and one working conversation.
Projects, tasks (including subtasks), repositories, branches, and living documents
each have a canonical room. They are **facets** of that room, not mutually
exclusive room types. An ordinary conversation can have no object facets.

One object has one conversation home. One room can be home to several objects.
A task and its implementation branch can therefore be two views of **the same
channel UUID**, not separate channels with synchronized messages. Switching from
task details to the diff changes the view, not the conversation. Adding a facet
does not create another chat.

Object state stays in its own representation: a task has status and assignees,
a repository has refs, and a document has content. The room brings these views
and their conversation together.

## Projects and Repositories

Projects organize a time-bound outcome. Repositories are durable homes for code
and ongoing conversation. A project can span several repositories, and a
repository can support many projects over time or concurrently. Their rooms have
different purposes and do not normally share a one-to-one relationship.

For example, a **Simplify sign-in** project spans the identity service and mobile
app repositories:

```text
Project room: Simplify sign-in
├── Task room: Improve account recovery
│   ├── Room S: Fix recovery tokens     [subtask + identity-service branch]
│   └── Room M: Add recovery screen     [subtask + mobile-app branch]
└── Design room: Recovery experience   [document]

Repository room: identity-service      [ongoing code and maintenance discussion]
└── Branch view: fix-recovery-tokens → room S

Repository room: mobile-app            [ongoing code and maintenance discussion]
└── Branch view: recovery-screen → room M
```

The subtask and branch in S open the same room; likewise for M. The repository
branch listings link to those rooms rather than creating new conversations.
Completing the sign-in project does not end either repository's ongoing work.

## Nested Rooms and Relationships

Rooms nest to organize work: a project contains task rooms, a task contains
subtask rooms, and a document can have its own room or be a view of an existing
room. A channel canvas is the document view within a room.

A room has at most one containment parent. The navigation tree is not the entire
relationship model: a task's room can sit under its project while its branch
still belongs to a repository elsewhere. Project-to-repository links and
branch-to-repository links remain available independently of that tree. Multiple
routes to a room all lead to the same conversation.

## Access

A room and its object views should feel like one workspace. Someone admitted to
that workspace should be able to read its conversation and the objects presented
as part of it, rather than encountering an arbitrary inaccessible subset. The
same principle applies when presenting nested rooms as a shared project workspace.
Reading the work and permission to push, merge, or administer it are distinct
capabilities.

The permissions design must make that experience consistent across linked rooms
and objects. For repositories, it must choose whether access is governed through
the canonical room relationship or through repository permissions coordinated
with the room. The model does not prescribe a separate, unrelated channel link
just for Git authorization. The choice of permission authority and inheritance
rules remains an explicit design decision; the required user experience is
coherent access, not disconnected permission islands.

## Backend Specification

Persist room relationships on the channel:

- `parent_channel_id`: nullable channel UUID, in the same community, indexed for
  child lookup.
- `object_bindings`: a list of typed object references stored as JSONB. A room
  can bind several objects, including a task and its branch.

Bindings reference the object they represent: an issue ID for a task, an
addressable project or repository coordinate, or a repository coordinate plus
full Git ref for a branch. Object records retain their own state; the bindings
connect them to their conversation home.

Expose these fields through signed channel create (`9007`), edit (`9002`), and
metadata discovery (`39000`) events. No feature-specific HTTP endpoint is needed.
Edits can set or clear the parent and replace the binding list. Omitted fields
remain unchanged; parent clearing and an empty binding list are explicit actions.
Persist a metadata edit atomically and return the same relationships on discovery.

The relay validates management authority, bounded binding shapes, and an existing
same-community parent. It rejects self-parenting and cycles, including concurrent
moves that would create a cycle.

All clients use the same relationships to resolve an object's canonical room.
Creating another view reuses that room. Repeated creation requests must converge
on the same room rather than create parallel conversations. Conflicting bindings
must be reconciled consistently across clients, not resolved by local discovery
order. The metadata storage does not require a global unique-binding constraint.
