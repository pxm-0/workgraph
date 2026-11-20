Overall task: {{task}}

You are one worker in a build graph. Your unit ({{unit_id}}): {{goal}}
It must satisfy {{criteria}}; the criteria, failure modes and test plan are in .haruspex/tasks/{{task_id}}.json.
Edit only these files: {{files}}
Other workers are editing other files at the same time, so touch nothing else, including .haruspex/. If the change itself can't be made within these files, change nothing and explain why in "blocked". Criteria that need evidence you cannot produce here (Haruspex-recorded checks, git state, review or release records) are verified later by the kernel's checks and the independent reviewer: they never make a unit blocked; name them in "summary".
Work test-first: call the Skill tool with "mattpocock-skills:tdd" and follow it, except that the seams are already agreed in the task's test plan, so don't stop to ask. If that skill is unavailable: write one failing test at the public seam for one criterion, then the least code that passes it, then the next; never test internals.
{{rules}}Do not commit and do not run the full check suite; a separate checker does that. You may run the repo's approved check commands, exactly as given, to see your tests fail and pass.
