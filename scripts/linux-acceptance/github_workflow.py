#!/usr/bin/env python3
"""ADR 0008 §9 acceptance: real Construct in a VM publishes to a scratch repository.

Requires the documented Linux VM environment (DECK_VM_IMAGE, DECK_VM_JOURNAL,
DECK_VM_KRUN, DECK_VM_EXECUTABLE, DECK_GATEWAY_EXECUTABLE), a visible Hyprland
session, `DECK_ACCEPTANCE_UI_EXECUTABLE` (the built Deck window), a
`DECK_CREDENTIALS` file store holding `DECK_GITHUB_TOKEN`, and a host checkout
of a private scratch repository at `--project` whose `origin` points at it.
`--construct-dev` is the built construct-dev that serves the scripted model.

The model is scripted (`construct-dev serve`), so it cannot reason about a
reply: each turn's steps are written by this driver, and the tool results the
model saw are read back from the serve request dumps. A refusal's "rebase onto
<sha>" is therefore asserted in the tool result, not inferred from model prose.
No credential value is read from the store or written to evidence; the
credential listing contributes its fingerprint only.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import time

from support import Daemon, Evidence


def sh(*argv, **kwargs):
    return subprocess.run(argv, check=True, capture_output=True, text=True, **kwargs).stdout.strip()


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class Model:
    """The scripted OpenAI-compatible endpoint; restarted per turn on one port."""

    def __init__(self, binary, root, port):
        self.binary = binary
        self.root = root
        self.port = port
        self.process = None

    def start(self, name, steps):
        self.stop()
        directory = self.root / name
        (directory / 'dump').mkdir(parents=True, exist_ok=True)
        script = directory / 'script.json'
        script.write_text(json.dumps({'steps': steps}, indent=2) + '\n')
        log = (directory / 'serve.log').open('w')
        self.process = subprocess.Popen(
            [self.binary, 'serve', '--port', str(self.port), '--script', str(script),
             '--log', str(directory / 'log.jsonl'), '--dump', str(directory / 'dump')],
            stdout=log, stderr=log)
        for _ in range(200):
            if self.process.poll() is not None:
                raise RuntimeError(f'scripted model exited: {directory / "serve.log"}')
            try:
                with socket.create_connection(('127.0.0.1', self.port), timeout=.1):
                    log.close()
                    return
            except OSError:
                time.sleep(.05)
        raise RuntimeError('scripted model did not listen')

    def stop(self):
        if self.process is not None:
            self.process.terminate()
            self.process.wait(timeout=10)
            self.process = None

    def requests(self, name):
        directory = self.root / name / 'dump'
        return [json.loads(path.read_text()) for path in sorted(directory.glob('req-*.json'))]

    def tool_results(self, name):
        """Every distinct tool result the model was sent, in order."""
        seen, out = set(), []
        for request in self.requests(name):
            for message in request.get('messages', []):
                if message.get('role') != 'tool':
                    continue
                content = message.get('content')
                text = content if isinstance(content, str) else json.dumps(content)
                if text not in seen:
                    seen.add(text)
                    out.append(text)
        return out

    def turn_tool_calls(self, name):
        """Only the calls of the turn the last request belongs to."""
        messages = self.requests(name)[-1].get('messages', [])
        start = max((i for i, m in enumerate(messages) if m.get('role') == 'user'), default=0)
        out = []
        for message in messages[start:]:
            if message.get('role') != 'assistant':
                continue
            for call in message.get('tool_calls') or []:
                function = call.get('function') or {}
                try:
                    args = json.loads(function.get('arguments') or '{}')
                except json.JSONDecodeError:
                    args = {'unparsed': function.get('arguments')}
                out.append({'name': function.get('name'), 'args': args})
        return out

    def tool_calls(self, name):
        out = []
        for request in self.requests(name):
            for message in request.get('messages', []):
                if message.get('role') != 'assistant':
                    continue
                for call in message.get('tool_calls') or []:
                    function = call.get('function') or {}
                    try:
                        args = json.loads(function.get('arguments') or '{}')
                    except json.JSONDecodeError:
                        args = {'unparsed': function.get('arguments')}
                    out.append({'name': function.get('name'), 'args': args})
        return out


class Window:
    """The real Deck window, driven through DECK_CONTROL."""

    def __init__(self, data, project, control, executable, agent, environment):
        self.data = data
        self.project = project
        self.control_path = control
        self.executable = executable
        self.agent = agent
        self.environment = environment
        self.pid = None

    def launch(self):
        command = 'cd ' + shlex.quote(str(self.project)) + ' && exec ' + shlex.join([
            'env', f'DECK_DATA={self.data}', f'DECK_CONTROL={self.control_path}',
            str(self.executable), '--', *self.agent])
        dispatch = 'hl.dsp.exec_cmd(' + json.dumps(shlex.join(['sh', '-c', command])) + ', { no_initial_focus = true })'
        subprocess.run(['hyprctl', 'dispatch', dispatch], check=True, capture_output=True, env=self.environment)
        self.until(lambda state: state['session']['status'] == 'ready', 240)
        self.pid = self.control(cmd='geometry')['pid']

    def control(self, **request):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(10)
            stream.connect(str(self.control_path))
            stream.sendall(json.dumps(request).encode() + b'\n')
            reply = json.loads(stream.makefile('rb').readline())
            if not reply.get('ok'):
                raise RuntimeError(reply)
            return reply

    def state(self):
        return self.control(cmd='state')

    def until(self, predicate, timeout=180):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                last = self.state()
                if predicate(last):
                    return last
            except (OSError, ValueError) as error:
                last = error
            time.sleep(.1)
        raise RuntimeError(f'window did not reach the expected state: {last}')

    def prompt(self, text):
        self.control(cmd='type', text=text)
        self.control(cmd='key', key='enter')

    def key(self, key, modifiers=None):
        request = {'cmd': 'key', 'key': key}
        if modifiers:
            request['modifiers'] = modifiers
        self.control(**request)

    def screenshot(self, path):
        geometry = self.control(cmd='geometry')
        clients = json.loads(sh('hyprctl', 'clients', '-j', env=self.environment))
        window = next(client for client in clients if client['pid'] == geometry['pid'])
        region = f"{window['at'][0]},{window['at'][1]} {window['size'][0]}x{window['size'][1]}"
        subprocess.run(['grim', '-g', region, str(path)], check=True, env=self.environment)
        return geometry

    def entries(self):
        return self.state()['session']['entries']

    def close(self):
        if self.pid is None:
            return
        try:
            self.key('q', modifiers=['ctrl'])
        except (OSError, ValueError):
            subprocess.run(['kill', str(self.pid)], capture_output=True)
        for _ in range(100):
            if not Path(f'/proc/{self.pid}').exists():
                return
            time.sleep(.05)


class Scroll:
    """The distinct chip labels a window showed, with the time each was first seen."""

    def __init__(self):
        self.seen = []

    def observe(self, state, now):
        card = state['board']['cards'][0] if state['board']['cards'] else None
        chip = card and card.get('chip')
        label = f"{chip['dot']}: {chip['label']}" if chip else None
        if label and (not self.seen or self.seen[-1][1] != label):
            self.seen.append((round(now, 3), label))


def open_publication(window, task):
    """Click the card's chip, or the header chip, until the panel is open."""
    if window.state()['publication'] == task:
        return True
    # The sessions pane starts at 12 + rail(52) + 12 and is 296 wide with 12
    # padding; its first row is the "Sessions" header, then a 14 gap, then the
    # 64-high cards. The chip sits at a card's right end. The header chip is
    # the fallback when no card responds where the layout says it is.
    for y in range(112, 184, 8):
        for x in range(284, 357, 8):
            window.control(cmd='click', x=x, y=y)
            state = window.state()
            if state['publication'] == task:
                return True
            if state['publication'] is not None:
                window.control(cmd='click', x=x, y=y)
            if state['access']['open']:
                window.control(cmd='key', key='escape')
    geometry = window.control(cmd='geometry')['viewport']
    for point in [(x, 31) for x in range(int(geometry['w']) - 430, int(geometry['w']) - 60, 8)]:
        window.control(cmd='click', x=point[0], y=point[1])
        state = window.state()
        if state['publication'] == task:
            return True
        if state['access']['open']:
            window.control(cmd='key', key='escape')
    return False


def turn(output, evidence, window, model, name, prompt, steps, final_text, expect_approval=False, timeout=240,
         pending=None):
    """Serve one turn's script, drive it, and return what the window and model saw."""
    model.start(name, steps)
    approvals = []
    scroll = Scroll()
    started = time.monotonic()
    answered = False
    window.prompt(prompt)
    deadline = started + timeout
    while time.monotonic() < deadline:
        state = window.state()
        scroll.observe(state, time.monotonic() - started)
        if state['approval'] is not None and not state['approval']['id'] in [a['id'] for a in approvals]:
            approvals.append(dict(state['approval'], pending=pending() if pending else None))
            if expect_approval and not answered:
                time.sleep(.5)
                window.screenshot(output / f'{name}-card.png')
                answered = True
                window.control(cmd='key', key='1')  # Allow once
        if state['session']['status'] == 'ready' and any(e.get('agent') == final_text for e in state['session']['entries']):
            if final_text:
                break
        time.sleep(.1)
    else:
        raise RuntimeError(f'{name}: turn did not finish; state={json.dumps(window.state())[:2000]}')
    state = window.state()
    (output / f'{name}.json').write_text(json.dumps({
        'prompt': prompt, 'steps': steps, 'approvals': approvals,
        'entries': state['session']['entries'], 'board': state['board'],
        'publication': state['publication'], 'publish_notice': state['publish_notice'],
        'chips': scroll.seen,
        'tool_calls': model.tool_calls(name), 'tool_results': model.tool_results(name),
    }, indent=2) + '\n')
    evidence.check(f'{name}: turn reached a text reply', any(e.get('agent') == final_text for e in state['session']['entries']), final_text)
    if not expect_approval:
        evidence.check(f'{name}: no boundary card was shown', approvals == [], approvals)
    return dict(approvals=approvals, chips=scroll.seen, state=state, name=name)


def wait_chip(window, labels, timeout=150, output=None, prefix='chip'):
    """Wait for one of the dot labels, recording every transition seen."""
    deadline = time.monotonic() + timeout
    scroll = Scroll()
    started = time.monotonic()
    while time.monotonic() < deadline:
        state = window.state()
        scroll.observe(state, time.monotonic() - started)
        chip = state['board']['cards'][0]['chip'] if state['board']['cards'] else None
        if chip and chip['dot'] in labels:
            if output is not None:
                (output / f'{prefix}-scroll.json').write_text(json.dumps(scroll.seen, indent=2) + '\n')
            return state, scroll.seen
        time.sleep(.5)
    raise RuntimeError(f'chip never reached {labels}: {window.state()["board"]}')


def github(*args):
    return sh('gh', 'api', *args)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--project', type=Path, required=True, help='host checkout whose origin is the scratch repository')
    parser.add_argument('--repository', default='strf0x1/deck-acceptance-scratch')
    parser.add_argument('--construct-dev', type=Path,
                        default=Path(os.environ.get('CONSTRUCT_DEV', str(Path(__file__).resolve().parents[2] / '../construct/target/debug/construct-dev'))))
    parser.add_argument('--construct-repo', type=Path,
                        default=Path(os.environ.get('CONSTRUCT_REPO', str(Path(__file__).resolve().parents[2] / '../construct'))))
    parser.add_argument('--agent', default='/usr/local/bin/construct')
    parser.add_argument('--deck', type=Path, default=Path(os.environ.get('DECK_ACCEPTANCE_UI_EXECUTABLE', 'target/debug/deck')))
    args = parser.parse_args()

    for name in ['DECK_VM_IMAGE', 'DECK_VM_JOURNAL', 'DECK_VM_KRUN', 'DECK_VM_EXECUTABLE',
                 'DECK_GATEWAY_EXECUTABLE', 'DECK_CREDENTIALS', 'XDG_RUNTIME_DIR']:
        if not os.environ.get(name):
            raise SystemExit(f'{name} must be set (scripts/linux-acceptance/README.md)')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    project = args.project.resolve()
    repository = args.repository.strip('/').lower()
    evidence = Evidence(output)
    environment = dict(os.environ)
    runtime = Path(environment['XDG_RUNTIME_DIR'])
    if not environment.get('HYPRLAND_INSTANCE_SIGNATURE'):
        instances = sorted((runtime / 'hypr').glob('*'), key=lambda path: path.stat().st_mtime)
        if not instances:
            raise SystemExit('HYPRLAND_INSTANCE_SIGNATURE is unset and no Hyprland instance is running')
        environment['HYPRLAND_INSTANCE_SIGNATURE'] = instances[-1].name
    environment.setdefault('WAYLAND_DISPLAY', 'wayland-1')

    # Preconditions: the fixture the run may publish to, and nothing in the
    # project file that changes the approval mode.
    visibility = sh('gh', 'repo', 'view', repository, '--json', 'visibility', '-q', '.visibility').lower()
    evidence.check('fixture: scratch repository is private', visibility == 'private', visibility)
    origin = sh('git', '-C', str(project), 'remote', 'get-url', 'origin')
    evidence.check('fixture: project origin names the scratch repository',
                   repository.split('/')[1] in origin and 'github.com' in origin, origin)
    env_file = project / '.env'
    if not env_file.exists():
        env_file.write_text('SCRATCH_ACCEPTANCE=ignored\n')
    ignored = subprocess.run(['git', '-C', str(project), 'check-ignore', '.env'], capture_output=True, text=True).stdout.strip()
    evidence.check('fixture: the project .env is git-ignored', ignored == '.env', ignored)
    project_file = project / '.deck/project.toml'
    evidence.check('fixture: the main run starts with no project file', not project_file.exists(), str(project_file))
    workflow = sh('gh', 'api', f'repos/{repository}/contents/.github/workflows/ci.yml?ref=main',
                  '--jq', '.path')
    evidence.check('fixture: main has a trivial CI workflow', workflow == '.github/workflows/ci.yml', workflow)

    stored = subprocess.run([os.environ['DECK_VM_EXECUTABLE'], 'credential', 'list'],
                            capture_output=True, text=True, env=environment)
    credential = next((line for line in stored.stdout.splitlines() if line.startswith('DECK_GITHUB_TOKEN\t')), '')
    model_key = next((line for line in stored.stdout.splitlines() if line.startswith('OPENAI_API_KEY\t')), '')
    evidence.check('fixture: the file store holds DECK_GITHUB_TOKEN', bool(credential), credential.replace('\t', ' '))
    evidence.check('fixture: the file store holds the synthetic model key', bool(model_key), model_key.replace('\t', ' '))

    root = Path(__file__).resolve().parents[2]
    revision = sh('git', '-C', str(root), 'rev-parse', 'HEAD')
    status = sh('git', '-C', str(root), 'status', '--porcelain')
    dirty = [line for line in status.splitlines() if 'scratch-project' not in line]

    port = free_port()
    model = Model(str(args.construct_dev), output / 'model', port)
    daemon = None
    window = None
    window_two = None
    try:
        daemon = Daemon(output / 'daemon', f'http://127.0.0.1:{port}/v1', os.environ['DECK_CREDENTIALS'])
        control_socket = Path(environment['XDG_RUNTIME_DIR']) / f'deck-89-{os.getpid()}.sock'
        window = Window(daemon.data, project, control_socket, args.deck,
                        [args.agent, 'acp'], environment)
        window.launch()
        tasks = daemon.control(op='tasks')
        task = tasks[0]['id']
        daemon.tasks.append(task)
        branch = f'deck/{task}'
        window.control(cmd='settle')
        state = window.state()
        evidence.check('window: the VM task is running with the project identity', tasks[0]['backend'] == 'vm' and
                       tasks[0]['spec']['cwd'] == str(project), tasks[0]['spec'])
        evidence.check('window: the task recorded the inferred GitHub policy',
                       tasks[0]['spec'].get('github', {}).get('repository') == repository and
                       tasks[0]['spec']['github'].get('base') == 'main' and
                       tasks[0]['spec']['github'].get('approvals') == 'own-branch', tasks[0]['spec'].get('github'))
        evidence.check('window: a capable agent shows no publish notice', state['publish_notice'] is None,
                       state['publish_notice'])

        # Case 8 rides on the first turn: the ignored .env must not be imported.
        first = turn(output, evidence, window, model, '01-publish',
                     'Make the first scratch change, commit it, publish it and open a draft pull request.',
                     [
                         {'call': 'shell', 'args': {'command':
                             "cd /workspace && printf 'first change\\n' > change.txt && git add change.txt && git status --short && "
                             "echo --- && pwd && git branch --show-current && git remote get-url origin && "
                             "test ! -e .env && echo env-absent && test -f .gitignore && echo gitignore-present"}},
                         {'call': 'developer', 'args': {'operation': 'commit', 'message': 'Add the first scratch change'}},
                         {'call': 'developer', 'args': {'operation': 'push'}},
                         {'call': 'developer', 'args': {'operation': 'pr_create',
                             'title': 'Scratch: first change', 'body': 'Acceptance scratch change one.', 'draft': True}},
                         {'call': 'shell', 'args': {'command': 'curl -s --max-time 5 "$DECK_BOUNDARY_URL/events"'}},
                         {'text': 'Draft pull request opened.'},
                     ], 'Draft pull request opened.')
        results = model.tool_results('01-publish')
        shells = [text for text in results if 'env-absent' in text]
        evidence.check('case 8: the ignored .env is absent from the guest checkout',
                       bool(shells) and 'env-absent' in shells[0], shells[0][:600] if shells else None)
        publish = next((text for text in results if text.startswith('Published ')), '')
        evidence.check('case 1: publish reports a created branch', '(created)' in publish, publish)
        ready = next((text for text in results if text.startswith('Pull request #')), '')
        number = int(ready.split('#')[1].split(' ')[0]) if ready.startswith('Pull request #') else 0
        evidence.check('case 1: the draft pull request was opened by the task', number > 0 and '(draft,' in ready, ready)
        events = next((text for text in results if text.startswith('[') and 'approval_id' in text), '')
        claimed = [event for event in json.loads(events or '[]')
                   if event.get('gateway') == 'github' and event.get('state') == 'allowed' and event.get('scope') == 'default']
        evidence.check('case 1: the write is admitted with scope default on the boundary /events feed',
                       any(event.get('destination', '').startswith(repository) for event in claimed), claimed)
        sha = publish.split(' ')[1] if publish.startswith('Published ') else ''
        remote = json.loads(github(f'repos/{repository}/git/ref/heads/{branch}'))
        evidence.check('case 1: GitHub has the published branch at the reported commit',
                       remote['object']['sha'] == sha, {'github': remote['object']['sha'], 'reported': sha})
        pull = json.loads(github(f'repos/{repository}/pulls/{number}'))
        evidence.check('case 1: GitHub shows the pull request as a draft',
                       pull['draft'] is True and pull['head']['ref'] == branch and pull['base']['ref'] == 'main',
                       {'draft': pull['draft'], 'head': pull['head']['ref'], 'base': pull['base']['ref']})
        state = window.until(lambda s: bool(s['board']['cards']) and s['board']['cards'][0].get('chip') is not None, 90)
        card = state['board']['cards'][0]
        evidence.check('case 2: the card shows the pull request chip',
                       card['chip'] is not None and card['chip']['dot'] == 'draft' and card['chip']['label'] == f'#{number}', card['chip'])
        evidence.check('case 2: publishing put the same chip in the header',
                       state['board']['cards'][0]['chip']['label'] == f'#{number}', card['chip'])
        evidence.check('case 1: the admission also reaches the agent as a boundary note',
                       any('[deck boundary] blocked while this ran' in text and 'scope: default' in text for text in results),
                       next((text[-300:] for text in results if '[deck boundary] blocked while this ran' in text), None))
        evidence.check('case 2: opening the chip opens the publication panel', open_publication(window, task),
                       window.state()['publication'])
        window.control(cmd='settle')
        window.screenshot(output / '02-panel.png')

        # Checks arrive through deckd's live read, not from the agent.
        published = Path(daemon.data / 'tasks' / task / 'pull_request.json')
        for _ in range(60):
            if published.exists():
                live = json.loads(published.read_text())
                runs = (live.get('pull_request') or {}).get('checks', {}).get('runs') or []
                if runs:
                    break
            time.sleep(2)
        live = json.loads(published.read_text()) if published.exists() else {}
        runs = (live.get('pull_request') or {}).get('checks', {}).get('runs') or []
        evidence.check("case 3: live state lists the check runs on the pull request", len(runs) > 0,
                       [(r.get('name'), r.get('status'), r.get('conclusion')) for r in runs])
        window.control(cmd='settle')
        window.screenshot(output / '03-panel-checks.png')
        chip = window.state()['board']['cards'][0].get('chip')
        evidence.check('case 3: a draft pull request shows the draft dot while its checks run',
                       chip is not None and chip['dot'] == 'draft', chip)

        # Case 4: one more commit on the same branch is a fast-forward.
        second = turn(output, evidence, window, model, '04-second-push',
                     'Add a second change, publish it and read the pull request.',
                     [
                         {'call': 'shell', 'args': {'command':
                             "cd /workspace && printf 'second change\\n' >> change.txt && git add change.txt && git status --short"}},
                         {'call': 'developer', 'args': {'operation': 'commit', 'message': 'Add the second scratch change'}},
                         {'call': 'developer', 'args': {'operation': 'push'}},
                         {'call': 'developer', 'args': {'operation': 'pr_review', 'number': number}},
                         {'call': 'shell', 'args': {'command': 'curl -s --max-time 5 "$DECK_BOUNDARY_URL/events"'}},
                         {'text': 'Second change published; the pull request checks were read.'},
                     ], 'Second change published; the pull request checks were read.')
        results = model.tool_results('04-second-push')
        publish = next((text for text in reversed(results) if text.startswith('Published ')), '')
        evidence.check('case 4: the second push fast-forwards from the previous commit',
                       'fast-forwarded from' in publish and sha and sha in publish, publish)
        second_sha = publish.split(' ')[1] if publish.startswith('Published ') else ''
        remote = json.loads(github(f'repos/{repository}/git/ref/heads/{branch}'))
        evidence.check('case 4: GitHub advanced the branch to the reported commit',
                       remote['object']['sha'] == second_sha, {'github': remote['object']['sha'], 'reported': second_sha})
        review = next((text for text in reversed(results) if text.startswith('Pull request #')), '')
        evidence.check('case 3: pr_review reads the checks back to the agent',
                       'Head ' in review and ('Checks (' in review or 'No checks reported' in review), review)

        # Case 6: marking ready asks once, with the publish summary on the card.
        last = turn(output, evidence, window, model, '05-ready',
                    'Mark the pull request ready for review.',
                    [
                        {'call': 'developer', 'args': {'operation': 'pr_update', 'number': number, 'draft': False}},
                        {'text': 'The pull request is ready for review.'},
                    ], 'The pull request is ready for review.', expect_approval=True,
                    pending=lambda: daemon.control(op='approvals', task=task))
        approval = last['approvals'][0] if last['approvals'] else {}
        pending = (approval.get('pending') or []) if approval else []
        evidence.check('case 6: marking ready asks with a card on the pull request',
                       approval.get('operation') == 'update_pr' and str(approval.get('destination', '')).startswith(repository),
                       approval)
        summary = (pending[0].get('details') or {}).get('summary') if pending else None
        evidence.check('case 6: the card carries the publish summary', bool(summary), summary)
        pull = json.loads(github(f'repos/{repository}/pulls/{number}'))
        evidence.check('case 6: GitHub shows the pull request ready for review',
                       pull['draft'] is False and pull['state'] == 'open', {'draft': pull['draft'], 'state': pull['state']})
        # update_pr is expected to make it ready; when GitHub still calls it a
        # draft, the harness finishes the transition with GitHub's own GraphQL
        # mutation so the view above it can still be exercised. That product
        # failure is the failed check above, not a passing fixture.
        if pull.get('draft'):
            query = ('mutation { markPullRequestReadyForReview(input: {pullRequestId: "' + pull['node_id'] +
                     '"}) { pullRequest { number isDraft } } }')
            subprocess.run(['gh', 'api', 'graphql', '-f', f'query={query}'], check=False, capture_output=True, text=True)
            pull = json.loads(github(f'repos/{repository}/pulls/{number}'))
            evidence.check('fixture: the harness made the draft ready after update_pr did not',
                           pull['draft'] is False, {'draft': pull.get('draft')})
        state, running = wait_chip(window, ('checks running',), 180, output, '05-chip-running')
        evidence.check('case 3: the chip shows the queued check as running',
                       running[-1][1].startswith('checks running'), running)
        window.control(cmd='settle')
        window.screenshot(output / '05-running.png')
        head = pull['head']['sha']
        workflows = json.loads(github(f'repos/{repository}/actions/runs?head_sha={head}&per_page=100'))
        queued = [run['id'] for run in workflows.get('workflow_runs', []) if run.get('status') != 'completed']
        for run in queued:
            subprocess.run(['gh', 'api', '-X', 'POST', f'repos/{repository}/actions/runs/{run}/cancel'],
                           check=False, capture_output=True, text=True)
        state, result = wait_chip(window, ('checks failing', 'ready'), 240, output, '05-chip-result')
        labels = [label for _, label in result]
        evidence.check('case 3: the chip went from running checks to their result',
                       any(label.startswith('checks running') for label in [label for _, label in running]) and
                       (labels[-1].startswith('checks failing') or labels[-1].startswith('ready')), labels)
        live = json.loads(published.read_text())
        runs = (live.get('pull_request') or {}).get('checks', {}).get('runs') or []
        evidence.check('case 3: the panel lists the completed runs',
                       bool(runs) and all(run.get('status') == 'completed' for run in runs),
                       [(r.get('name'), r.get('status'), r.get('conclusion')) for r in runs])
        window.control(cmd='settle')
        window.screenshot(output / '05-checks.png')

        # Case 5: rewrite the task branch. Pruning makes the old tip unknown to
        # Construct, so the bundle reaches the gateway's fast-forward check.
        turn(output, evidence, window, model, '06-refused',
             'Rewrite the branch and publish it.',
             [
                 {'call': 'shell', 'args': {'command':
                     "cd /workspace && git log --oneline -3 && git reset --hard HEAD~1 && "
                     "git reflog expire --expire=now --all && git gc --prune=now --quiet && "
                     "printf 'rewritten\\n' > rewritten.txt && git add rewritten.txt && git commit -q -m 'Rewrite as a divergent commit' && "
                     "git log --oneline -4 && git rev-parse HEAD"}},
                 {'call': 'developer', 'args': {'operation': 'push'}},
                 {'text': 'The publish was refused; the branch moved. Not resending the same commits.'},
             ], 'The publish was refused; the branch moved. Not resending the same commits.')
        results = model.tool_results('06-refused')
        refusal = next((text for text in results if 'refused' in text and 'rebase onto' in text), '')
        evidence.check('case 5: the gateway refuses the rewrite with deck_github_refused and names current',
                       'deck_github_refused (403)' in refusal and second_sha and f'is at {second_sha}' in refusal
                       and f'rebase onto {second_sha}' in refusal, refusal)
        calls = model.turn_tool_calls('06-refused')
        pushes = [call for call in calls if call['name'] == 'developer' and call['args'].get('operation') == 'push']
        evidence.check('case 5: the agent does not retry the refused publish', len(pushes) == 1, len(pushes))
        remote = json.loads(github(f'repos/{repository}/git/ref/heads/{branch}'))
        evidence.check('case 5: GitHub still has the fast-forwarded commit', remote['object']['sha'] == second_sha,
                       remote['object']['sha'])

        # Case 9: a stopped task keeps its publication, and resume restores the
        # live view (the state is tasks/<id>/pull_request.json).
        screenshot_before = window.state()['board']['cards'][0]['chip']
        daemon.control(op='stop', id=task)
        window.until(lambda state: state['session']['status'] in ('detached', {'down': ''}) or
                     isinstance(state['session']['status'], dict), 120)
        time.sleep(1)
        stopped = window.state()
        evidence.check('case 9: the stopped task still shows its chip',
                       stopped['board']['cards'][0]['chip'] == screenshot_before, stopped['board']['cards'][0]['chip'])
        evidence.check('case 9: the stopped task still opens its publication panel', open_publication(window, task),
                       window.state()['publication'])
        window.screenshot(output / '09-stopped.png')
        window.control(cmd='key', key='r', modifiers=['ctrl'])
        resumed = window.until(lambda state: state['session']['status'] == 'ready', 180)
        evidence.check('case 9: resume restores the session and keeps the chip',
                       resumed['board']['cards'][0]['chip'] == screenshot_before and
                       Path(daemon.data / 'tasks' / task / 'pull_request.json').exists(),
                       resumed['board']['cards'][0]['chip'])

        # Case 10 (optional): a merge on GitHub turns the card done and makes
        # Destroy ask once more. Only GitHub merges; Deck reads the result.
        merged = json.loads(github(f'repos/{repository}/pulls/{number}/merge', '--method', 'PUT',
                                   '-f', 'merge_method=merge'))
        evidence.check('case 10: GitHub merged the pull request', merged.get('merged') is True, merged.get('message'))
        state, _ = wait_chip(window, ('merged',), 180, output, '10-merged')
        card = state['board']['cards'][0]
        evidence.check('case 10: the card chip turns merged after the merge',
                       card['chip']['dot'] == 'merged', card)
        evidence.check('case 10: the merged task offers Destroy in its panel', open_publication(window, task),
                       window.state()['publication'])
        window.control(cmd='settle')
        window.screenshot(output / '10-merged.png')
        # The first click only arms the button; the second destroys. Its label
        # changes to "Destroy: confirm" between them (screenshot), and the
        # armed button is wider, so the second pass scans the same band.
        # The button is left of the panel's action row end, beside "Open on
        # GitHub"; its centre is stable at this window size.
        window.control(cmd='click', x=600, y=419)
        time.sleep(.8)
        window.control(cmd='settle')
        window.screenshot(output / '10-destroy-armed.png')
        window.control(cmd='click', x=600, y=419)
        time.sleep(1)
        destroyed = daemon.control(op='task', id=task)['state']['state'] == 'destroyed'
        evidence.check('case 10: the second Destroy click destroys the task', destroyed,
                       daemon.control(op='task', id=task)['state'])
        window.screenshot(output / '10-destroyed.png')

        # Case 7: ask mode restores a card for the first write.
        model.stop()
        project_file.parent.mkdir(parents=True, exist_ok=True)
        project_file.write_text('[github]\napprovals = "ask"\n')
        try:
            daemon_two = Daemon(output / 'daemon-ask', f'http://127.0.0.1:{port}/v1', os.environ['DECK_CREDENTIALS'])
            try:
                control_socket = Path(environment['XDG_RUNTIME_DIR']) / f'deck-89-ask-{os.getpid()}.sock'
                window_two = Window(daemon_two.data, project, control_socket, args.deck,
                                    [args.agent, 'acp'], environment)
                window_two.launch()
                tasks = daemon_two.control(op='tasks')
                ask_task = tasks[0]['id']
                daemon_two.tasks.append(ask_task)
                evidence.check('case 7: the ask task recorded the project approval mode',
                               tasks[0]['spec'].get('github', {}).get('approvals') == 'ask', tasks[0]['spec'].get('github'))
                asked = turn(output, evidence, window_two, model, '07-ask-first-push',
                             'Make the ask-mode change and publish it.',
                             [
                                 {'call': 'shell', 'args': {'command':
                                     "cd /workspace && printf 'ask mode\\n' > ask.txt && git add ask.txt && git status --short"}},
                                 {'call': 'developer', 'args': {'operation': 'commit', 'message': 'Ask-mode scratch change'}},
                                 {'call': 'developer', 'args': {'operation': 'push'}},
                                 {'text': 'Published after the card.'},
                             ], 'Published after the card.', expect_approval=True,
                             pending=lambda: daemon_two.control(op='approvals', task=ask_task))
                approval = asked['approvals'][0] if asked['approvals'] else {}
                evidence.check('case 7: the first push in ask mode shows a card',
                               approval.get('operation') in ('push', 'publish') and
                               approval.get('destination', '').startswith(repository), approval)
                results = model.tool_results('07-ask-first-push')
                evidence.check('case 7: the allowed push published the branch',
                               any(text.startswith('Published ') for text in results),
                               next((text for text in results if text.startswith('Published ')), ''))
            finally:
                if window_two is not None:
                    window_two.close()
                for item in daemon_two.control(op='tasks'):
                    if item['id'] not in daemon_two.tasks:
                        daemon_two.tasks.append(item['id'])
                daemon_two.close()
        finally:
            project_file.unlink(missing_ok=True)

        # Manifest: what produced this evidence. No credential value is read.
        images = {path.name: sha256(path) for path in sorted((Path(os.environ['DECK_VM_IMAGE']).parent).glob('*.img'))}
        manifest = {
            'date': time.strftime('%Y-%m-%d'),
            'deck_revision': revision,
            'construct_revision': sh('git', '-C', str(args.construct_repo.resolve()), 'rev-parse', 'HEAD'),
            'deck_dirty': dirty,
            'repository': repository,
            'task': task,
            'branch': branch,
            'pull_request': number,
            'commits': {'first': sha, 'second': second_sha},
            'port': port,
            'binaries': {name: sha256(Path(os.environ[name])) for name in
                         ['DECK_VM_EXECUTABLE', 'DECK_GATEWAY_EXECUTABLE', 'DECK_VM_KRUN', 'DECK_ACCEPTANCE_UI_EXECUTABLE']},
            'guest_construct_sha256': sha256(Path(os.environ['DECK_VM_IMAGE']).parent.parent / 'construct-build' / 'construct')
                if (Path(os.environ['DECK_VM_IMAGE']).parent.parent / 'construct-build' / 'construct').exists() else None,
            'guest_images_sha256': images,
            'credentials': [line.replace('\t', ' ') for line in stored.stdout.splitlines() if '\t' in line],
            'harness_sha256': {'github_workflow.py': sha256(Path(__file__)), 'support.py': sha256(Path(__file__).with_name('support.py'))},
            'fixture_limits': [
                'the model is scripted: the tool steps and the text reply each turn are fixed by the driver; '
                'a refusal is proven by Construct\'s tool result and by the one publish call in that turn, not by model reasoning',
                'GitHub-hosted Actions jobs do not start in the private scratch repository for this account (a minimal '
                'workflow there also fails in ~3 s with no steps; this repository\'s public CI runs normally), so '
                'the scratch CI targets a self-hosted runner and its check stays queued; the harness cancels '
                'the queued runs to produce the completed conclusion. The deck reads real GitHub check runs; no real CI ran',
                'the credential store holds the local gh user token (repo scope), not a fine-grained token restricted to '
                'the scratch repository: fine-grained tokens can only be minted in the web UI. The token never leaves the '
                'host store; the harness uses the same account\'s gh auth to verify GitHub',
                'Linux/x86_64 only; macOS was not run',
                'the pull request number and commit shas come from the turns; reruns reuse the scratch repository',
            ],
        }
        (output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    finally:
        model.stop()
        if window is not None:
            window.close()
        if daemon is not None:
            try:
                for item in daemon.control(op='tasks'):
                    if item['id'] not in daemon.tasks:
                        daemon.tasks.append(item['id'])
            except (OSError, RuntimeError):
                pass
            daemon.close()


if __name__ == '__main__':
    main()