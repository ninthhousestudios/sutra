# Issue tracker: Yojana

Yojana is the active issue tracker for this repo. All issue operations map to yojana MCP tool calls.

## Tool call mappings

### Create an issue

```
yojana_task action=create project="sutra" title="<title>" description="<desc>"
  category="enhancement|bug|experiment"
  slice_type="AFK|HITL"
  acceptance_criteria=[{"text":"...","done":false}]
  tags=["<tag>",...]
```

New tasks start as `needs-triage`. Set fields you know; omit what you don't.

### Fetch a ticket

By human ID (preferred):
```
yojana_task action=get id="sutra/<N>"
```

For a shaped context bundle:
```
yojana_context task="sutra/<N>" shape="summary"
yojana_context task="sutra/<N>" shape="working"
```

Use `summary` for quick status checks. Use `working` when you need acceptance criteria, decisions, neighbor context, and conversation history.

### List / query issues

```
yojana_query project="sutra" status="<status>" category="<cat>" tag="<tag>"
```

All parameters are optional. Omit `project` for cross-project queries. Each result includes `ready` and `blocked` flags.

### Find ready tasks

```
yojana_ready project="sutra"
```

Returns tasks with status `ready-for-agent` or `ready-for-human` where all `depends_on` targets are done. Omit `project` for cross-project.

### Apply a triage label

```
yojana_task action=update id="sutra/<N>" status="<new-status>"
```

Valid statuses follow the triage label vocabulary (see `triage-labels.md`), plus execution states: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `in_progress`, `done`, `wontfix`.

Transitions are validated by the state machine — invalid transitions are rejected with an error.

### Update task fields

```
yojana_task action=update id="sutra/<N>"
  title="..." description="..." acceptance_criteria=[...] decisions=[...]
  implementation_plan="..." context_refs=[...]
```

Partial updates — only include fields you're changing.

### Add a comment / conversation message

```
yojana_task action=comment id="sutra/<N>" text="<message>" author="agent"
```

Appends to the task's conversation thread. Shows up in `working` context shape.

### Create dependency edges

```
yojana_edge action=create source="<uuid>" target="<uuid>" edge_type="depends_on"
```

Edge types: `depends_on`, `relates_to`, `supersedes`, `refines`, `motivated_by`. Cycle detection runs on `depends_on` edges.

### Delete an edge

```
yojana_edge action=delete id="<edge-uuid>"
```

## When a skill says "publish to the issue tracker"

Call `yojana_task action=create` with project slug `sutra`.

## When a skill says "fetch the relevant ticket"

Call `yojana_task action=get` or `yojana_context` with the task identifier. The user will normally pass the human ID (`sutra/N`) or UUID.
