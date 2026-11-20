Overall task: {{task}}

Your unit ({{unit_id}}): {{goal}}
Files you may edit: {{files}}
{{problem}}
This is fix round {{round}} of {{max_rounds}}.
Diagnose before you change code: call the Skill tool with "mattpocock-skills:diagnosing-bugs" and follow it; you already have the feedback loop above, so start at reproduce and minimise. If that skill is unavailable: reproduce with the failing command, minimise, form one hypothesis, confirm it, then fix the root cause.
{{rules}}
Fix it within your files. Never weaken, skip or delete a test or check to make it pass. If your unit did not cause the problem, change nothing and say so in "blocked".
