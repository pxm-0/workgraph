Overall task: {{task}}

Haruspex task {{task_id}} (.haruspex/tasks/{{task_id}}.json) holds a Reading another agent just wrote. Assume it is incomplete. Read the code paths it touches and find what it missed: unhappy paths, partial success, retries, concurrency, permissions, data transitions, recovery, rollout and rollback, and external contracts (formats, units, timezones, protocols) that something outside this repo decides.
Add each real gap to the task JSON as a new assumption, failure mode or acceptance criterion with the next free id. Generic worries are not gaps; tie each one to the actual code path. Never remove or weaken what is there.
Apply the auto-proceed policy in your system prompt.
Give every criterion you add to one of these units: {{units}}.
Do not edit source code. Afterwards `{{hx}} gate ready-to-build . --json` must still show only the pending approval. Return what you added, the criterion-to-unit assignments, and any escalations.
