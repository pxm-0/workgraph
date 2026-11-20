import json,sys
for line in open(sys.argv[1]):
    e=json.loads(line)
    if e['type']=='assistant':
        for c in e['message']['content']:
            if c['type']=='tool_use': print('USE',c['name'],json.dumps(c['input'])[:150])
    if e['type']=='user':
        for c in e['message']['content']:
            if isinstance(c,dict) and c.get('type')=='tool_result': print('RES',str(c.get('content'))[:200].replace('\n',' '))
    if e['type']=='result': print('RESULT',e['subtype'],e['is_error'],e['total_cost_usd'],[ (d['tool_name'],json.dumps(d['tool_input'])[:80]) for d in e['permission_denials']])
