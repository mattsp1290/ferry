#!/usr/bin/env python3
import base64, json, os, pathlib, shutil, subprocess, sys, uuid
root=pathlib.Path(os.environ['STUB_ROOT']); name=pathlib.Path(sys.argv[0]).name; args=sys.argv[1:]
identifier=uuid.uuid4().hex
record=root/'records'/identifier
record.with_suffix('.json').write_text(json.dumps({'tool':name,'argv':args,'env':dict(os.environ)}))
def stdin_record(data): record.with_suffix('.stdin').write_bytes(data)
def output(value): print(json.dumps(value) if isinstance(value,(dict,list)) else value)
def real():
    path=shutil.which(name,path=os.environ['STUB_REAL_PATH'])
    os.execv(path,[path]+args)
def config_read():
    if '--config' not in args: return
    path=args[args.index('--config')+1]
    data=sys.stdin.buffer.read() if path=='/dev/stdin' else pathlib.Path(path).read_bytes()
    stdin_record(data)
if name=='ssh':
    data=sys.stdin.buffer.read(); stdin_record(data)
    env=dict(os.environ,HOME=str(root/'home'),TMPDIR=str(root/'remote'))
    sys.exit(subprocess.run(['bash','-c',args[-1]],input=data,env=env).returncode)
if name in ('jq','base64','sed','awk','tr','cat'):
    def is_file(arg):
        try: return pathlib.Path(arg).is_file()
        except OSError: return False
    input_files=any(is_file(arg) for arg in args if not arg.startswith('-'))
    jq_null_input = any(
        option.startswith('-') and not option.startswith('--') and 'n' in option
        for option in args)
    consumes_text = name in ('sed', 'awk', 'tr', 'cat') and not input_files
    consumes_json = name == 'jq' and not jq_null_input and not input_files
    consumes = name == 'base64' or consumes_text or consumes_json
    if consumes:
        data=sys.stdin.buffer.read(); stdin_record(data)
        stream=open(record.with_suffix('.stdin'),'rb'); os.dup2(stream.fileno(),0)
    # Record descriptors independently without consuming them; tee stdin for
    # commands that actually consume it. File input is recorded by its reader.
    if name=='jq':
        for i,arg in enumerate(args):
            if arg=='--rawfile' and args[i+2].startswith('/dev/fd/'):
                fd=int(args[i+2].split('/')[-1]); data=os.read(fd,1048576)
                stdin_record(data)
                reader,writer=os.pipe(); os.write(writer,data); os.close(writer)
                os.set_inheritable(reader,True); args[i+2]='/dev/fd/'+str(reader)
    real()
if name=='python3': real()
if name=='git':
    if args[:2]==['status','--porcelain']: sys.exit(0)
    os.execv(os.environ['STUB_REAL_GIT'],[os.environ['STUB_REAL_GIT']]+args)
if name=='curl':
    config_read(); url=args[-1]
    if '/topics' in url: sys.stdout.write('{"topics":["ferry-mirror"]}\n200')
    elif 'api.github.com' in url: sys.stdout.write('{}\n200')
    else: sys.stdout.write('{"login":"ferry"}\n200')
    sys.exit(0)
if name=='cargo': sys.exit(0)
if name=='ferry':
    if args and args[0]=='refs': output('b'*40+'\trefs/heads/main')
    else: output('config ok: 1 repositories')
    sys.exit(0)
if name in ('build','docker'): sys.exit(0)
if name=='assemble':
    (root/'published').touch(); output(os.environ['STUB_DIGEST']); sys.exit(0)
if name=='crane':
    if 'version' in args: output('stub')
    elif 'digest' in args:
        if '/ferry/ferry:' in args[-1] and not (root/'published').exists():
            print('MANIFEST_UNKNOWN: manifest unknown',file=sys.stderr); sys.exit(1)
        output(os.environ['STUB_DIGEST'])
    elif 'config' in args: output({'os':'linux','architecture':'amd64'})
    elif 'manifest' in args: output({'schemaVersion':2})
    sys.exit(0)
clean=args[:]
for option in ('--kubeconfig','-n'):
    while option in clean:
        i=clean.index(option); del clean[i:i+2]
image={'repository':'localhost:5000/ferry/ferry','digest':os.environ['STUB_DIGEST'],'version':os.environ['STUB_COMMIT']}
if name=='helm':
    if clean[0]=='lint' and os.environ.get('STUB_LINT_FAIL'): sys.exit(1)
    if clean[0]=='template' or clean[0]=='lint':
        os.execv(os.environ['STUB_REAL_HELM'],[os.environ['STUB_REAL_HELM']]+args)
    if clean[0]=='version': output('v3.13.0')
    elif clean[0]=='list': output(json.loads(os.environ.get('STUB_RELEASES_JSON','[]')))
    elif clean[:2] == ['get', 'values']:
        output({
            'image': image,
            'datadog': {'transport': 'none'},
            'config': {
                'health': {'listen': '0.0.0.0:8080'},
                'forgejo': {'url': 'https://git.example.internal', 'username': 'ferry'},
                'sync': {'poll_interval_seconds': 300},
                'repos': [{'github': 'example-owner/example-repository',
                           'forgejo': 'example-owner/example-repository'}]}})
    elif clean[0]=='status': output({'version':1,'info':{'status':'deployed'}})
    elif clean[0]=='upgrade':
        if '--dry-run=server' in clean and os.environ.get('STUB_DRY_RUN_FAIL'): sys.exit(1)
        if '--dry-run=server' in clean: output('kind: Deployment\nmetadata:\n  name: ferry\ndata:\n  secret: '+os.environ.get('STUB_DRY_SECRET',''))
        elif os.environ.get('STUB_UPGRADE_FAIL'): sys.exit(1)
        else: (root/'upgraded').touch()
    else: sys.exit('Unhandled helm call '+str(clean))
    sys.exit(0)
if name=='kubectl':
    if clean[0]=='apply':
        if clean[-1]=='-':
            data=sys.stdin.buffer.read(); stdin_record(data)
            if b'"kind":"Secret"' in data or b'"kind": "Secret"' in data:
                state=root/'secret-present'
                if not state.exists(): state.write_text('1')
                elif os.environ.get('STUB_SECRET_CHANGED'): state.write_text(str(int(state.read_text())+1))
        elif '-f' in clean:
            data=pathlib.Path(clean[clean.index('-f')+1]).read_bytes()
            parsed=json.loads("\n".join(line for line in data.decode().splitlines() if line != "---" and not line.startswith("# Source:")))
            if parsed.get('kind')=='List': (root/'preflight-pod.json').write_bytes(data)
        output('secret/ferry-credentials serverside-applied')
    elif clean[0] in ('delete','rollout'): pass
    elif clean[0]=='logs':
        if clean[1].startswith('ferry-preflight'):
            first=os.environ.get('STUB_PREFLIGHT_PVC_FAIL_ONCE') and not (root/'pvc-attempt').exists()
            (root/'pvc-attempt').touch()
            output('\n'.join(('FAIL:' if (key=='pvc' and first) or (key=='sockets' and (os.environ.get('STUB_SOCKET_FAIL') or os.environ.get('STUB_SOCKET_DIR_MISSING'))) or (key=='service' and os.environ.get('STUB_SERVICE_FAIL')) else 'ok:')+key for key in ['version','lfs','token','pvc','tls','sockets','service']))
        else: output('ferry started')
    elif clean[:2]==['get','namespace']: output({'metadata':{'labels':{} if os.environ.get('STUB_NAMESPACE_UNOWNED') else {'app.kubernetes.io/managed-by':'ferry-deploy'}}})
    elif clean[:2]==['get','secret']:
        if (root/'secret-present').exists(): output((root/'secret-present').read_text())
    elif clean[:2]==['get','events']:
        pod=json.loads((root/'preflight-pod.json').read_text()) if (root/'preflight-pod.json').exists() else {'items':[]}
        sockets=any(v['name']=='datadog-socket' for o in pod['items'] if o['kind']=='Pod' for v in o['spec']['volumes'])
        if sockets and os.environ.get('STUB_SOCKET_DIR_MISSING'): output('MountVolume.SetUp failed for volume "datadog-socket" : hostPath type check failed: /var/run/datadog is not a directory')
    elif clean[:2]==['get','pod']:
        pod=json.loads((root/'preflight-pod.json').read_text()) if (root/'preflight-pod.json').exists() else {'items':[]}
        sockets=any(v['name']=='datadog-socket' for o in pod['items'] if o['kind']=='Pod' for v in o['spec']['volumes'])
        if any('.status.phase' in a for a in clean): output('Pending' if sockets and os.environ.get('STUB_SOCKET_DIR_MISSING') else 'Succeeded')
        elif any('jsonpath=' in a for a in clean): pass
        else: output({'status':{'phase':'Succeeded'}})
    elif clean[:2]==['get','nodes']: output({'items':[{'metadata':{'name':'example-node'}}]})
    elif clean[:2]==['get','deployment']: output('ferry')
    elif clean[:2]==['get','pods']:
        if any('jsonpath=' in a for a in clean):
            if any('startTime' in a for a in clean): output('ferry-pod\t2026-01-01T00:00:00Z')
            else: output('new' if (root/'upgraded').exists() and not os.environ.get('STUB_KEEP_UID') else 'old')
            sys.exit(0)
        pod = {
            'metadata': {'name': 'ferry-pod', 'uid': 'new' if (root/'upgraded').exists() else 'old'},
            'spec': {'nodeName': 'example-node', 'containers': [{
                'name': 'ferry', 'image': image['repository']+'@'+image['digest'],
                'env': [{'name': 'DD_VERSION', 'value': image['version']}],
            }]},
            'status': {
                'phase': 'Running', 'startTime': '2026-01-01T00:00:00Z',
                'conditions': [{'type': 'Ready', 'status': 'True'}],
            },
        }
        output({'items': [pod]})
    else: sys.exit('Unhandled kubectl '+str(clean))
    sys.exit(0)
sys.exit('Unhandled stub '+name)
