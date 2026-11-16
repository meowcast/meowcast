#!/usr/bin/env python3
"""Host tests of meow-thermald (thermald.c): the state machine against a fake sensor, a fake
actions script and short times. Builds the daemon with the host compiler.

    python3 test-thermald.py [-v]
"""
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
VERBOSE = '-v' in sys.argv


class Rig:
    def __init__(self, tmp, **conf):
        self.dir = Path(tmp)
        self.run = self.dir / 'run'
        self.sensor = self.dir / 'temp'
        self.calls = self.dir / 'calls'
        self.failed = self.dir / 'failed'
        self.calls.write_text('')
        actions = self.dir / 'actions'
        actions.write_text(f'echo "$@" >> {self.calls}\n[ -e {self.dir}/hang ] && [ "$1" = enter ] && sleep 30\nexit 0\n')
        base = dict(sensor=self.sensor, run_dir=self.run, actions=actions, persist_log=self.dir / 'persist.log',
                    fail_cmd=f'echo failed > {self.failed}', poll_ms=50, poll_hot_ms=50, notice_s=1, cooldown_s=2,
                    settle_s=0, max_rounds=3, log_every_s=1)
        base.update(conf)
        self.conf = self.dir / 'thermal.conf'
        self.conf.write_text(''.join(f'{k}={v}\n' for k, v in base.items()))
        self.proc = None
        self.set(50)

    def set(self, celsius):
        tmp = self.dir / 'temp.new'
        tmp.write_text(f'{int(celsius * 1000)}\n')
        tmp.rename(self.sensor)

    def start(self):
        self.proc = subprocess.Popen([str(BIN), '-c', str(self.conf)], env={'PATH': os.environ['PATH']})
        self.wait(lambda: (self.run / 'state').exists(), 'state file')

    def state(self):
        try:
            text = (self.run / 'state').read_text()
        except FileNotFoundError:
            return {}
        if 'end=1' not in text:
            return {}
        return dict(line.split('=', 1) for line in text.splitlines() if '=' in line)

    def verbs(self):
        return [line.split()[0] for line in self.calls.read_text().splitlines() if line.strip()]

    def lines(self):
        return self.calls.read_text().splitlines()

    def log(self):
        try:
            return (self.run / 'log').read_text()
        except FileNotFoundError:
            return ''

    def wait(self, cond, what, timeout=8.0):
        end = time.time() + timeout
        while time.time() < end:
            if cond():
                return
            time.sleep(0.02)
        raise AssertionError(f'timed out waiting for {what}\nstate={self.state()}\ncalls={self.lines()}\nlog:\n{self.log()}')

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if VERBOSE:
            print(self.log())


def test_levels_and_hysteresis(r):
    r.start()
    assert r.state()['level'] == '0' and r.state()['state'] == 'normal'
    r.set(91)
    r.wait(lambda: r.state().get('level') == '1', 'level 1')
    r.set(100.5)
    r.wait(lambda: r.state().get('level') == '2', 'level 2')
    r.set(98)  # not below hot - hysteresis (97): stays
    time.sleep(0.4)
    assert r.state()['level'] == '2'
    r.set(105)
    r.wait(lambda: r.state().get('level') == '3', 'level 3')
    r.wait(lambda: 'hot' in r.state().get('notice', ''), 'the hot notice')
    r.set(96.5)  # below hot - 3: two levels down at once
    r.wait(lambda: r.state().get('level') == '1', 'level 1 again')
    r.set(80)
    r.wait(lambda: r.state().get('level') == '0', 'level 0')
    r.wait(lambda: 'level 0' in r.lines(), 'the level 0 action')
    seen = [l for l in r.lines() if l.startswith('level')]
    # (a hot level is applied again every log interval: drop the repeats)
    assert [l for i, l in enumerate(seen) if i == 0 or seen[i - 1] != l] == ['level 1', 'level 2', 'level 3', 'level 1', 'level 0'], r.lines()
    assert 'prepare' not in r.verbs()


def test_one_hot_sample_is_not_an_emergency(r):
    r = Rig(r.dir, confirm=3, poll_ms=200, poll_hot_ms=200)
    r.start()
    r.set(109)
    time.sleep(0.25)
    r.set(100)
    time.sleep(0.8)
    assert 'prepare' not in r.verbs() and r.state()['state'] == 'normal'
    return r


def test_emergency_and_resume(r):
    r.start()
    r.set(108.5)
    r.wait(lambda: r.state().get('state') == 'notice', 'notice state')
    assert (r.run / 'cooldown').exists(), 'marker from the notice on'
    r.wait(lambda: 'Too hot' in r.state().get('notice', ''), 'the notice')
    seq = int(r.state()['notice_seq'])
    assert 'enter' not in r.verbs(), 'the notice comes before the shutdown'
    r.wait(lambda: r.state().get('state') == 'cooldown', 'cooldown')
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    assert [l for l in r.lines() if not l.startswith('level')][0].startswith('prepare Too hot')
    r.set(99)  # not yet below resume (97) at the end of round 1
    r.wait(lambda: r.state().get('round') == '2', 'round 2')
    assert 'exit' not in r.verbs()
    r.set(90)
    r.wait(lambda: 'exit' in r.verbs(), 'exit', 6)
    r.wait(lambda: r.state().get('state') == 'normal' and 'Cooled down' in r.state().get('notice', ''), 'cooled notice')
    assert int(r.state()['notice_seq']) == seq + 1
    assert not (r.run / 'cooldown').exists()
    assert not r.failed.exists()
    persist = (r.dir / 'persist.log').read_text()
    assert 'EMERGENCY confirmed' in persist and 'COOLDOWN exit (cooled down)' in persist, persist
    assert r.verbs().count('enter') == 1 and r.verbs().count('exit') == 1
    # and it is armed again
    r.set(112)
    r.wait(lambda: r.verbs().count('enter') == 2, 'a second emergency')
    assert 'EMERGENCY at once' in r.log()


def test_never_cools_down(r):
    r.start()
    r.set(109)
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    r.set(100)  # stays between resume and the rise limit
    r.wait(lambda: r.failed.exists(), 'fail_cmd', 12)
    assert r.verbs()[-1] == 'fail' and 'exit' not in r.verbs()
    assert 'not cool enough after the last round' in r.log()
    assert (r.run / 'cooldown').exists(), 'nothing may start after a failure'
    assert r.state()['state'] == 'failed'


def test_heating_up_while_off(r):
    r.start()
    r.set(109)
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    r.set(111.5)  # more than 2 C above the start of the round, below panic
    r.wait(lambda: r.failed.exists(), 'fail_cmd', 6)
    assert 'still heating up' in r.log()
    assert r.state().get('round') == '1'


def test_panic_in_cooldown(r):
    r = Rig(r.dir, cooldown_s=20)
    r.start()
    r.set(109)
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    r.set(113.5)
    r.wait(lambda: r.failed.exists(), 'fail_cmd', 8)
    assert 'panic temperature' in r.log()
    return r


def test_emulation_only_raises(r):
    r.start()
    r.set(99)
    (r.run / 'emul_temp').write_text('20000\n')  # lower than the sensor: ignored
    time.sleep(0.3)
    r.wait(lambda: r.state().get('temp') == '99000', 'the real temperature')
    (r.run / 'emul_temp').write_text('109000\n')
    r.wait(lambda: 'enter' in r.verbs(), 'enter from the emulated temperature')
    (r.run / 'emul_temp').unlink()
    r.set(85)
    r.wait(lambda: 'exit' in r.verbs(), 'exit', 6)


def test_restart_continues_cooldown(r):
    r.start()
    r.set(109)
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    r.proc.kill()
    r.proc.wait()
    assert (r.run / 'cooldown').exists()
    r.set(90)
    r.proc = subprocess.Popen([str(BIN), '-c', str(r.conf)], env={'PATH': os.environ['PATH']})
    r.wait(lambda: 'exit' in r.verbs(), 'exit after the restart', 6)
    assert r.verbs().count('enter') == 2, 'enter is repeated (state unknown after a crash)'
    assert 'cooldown in progress' in r.log()


def test_sigterm_in_cooldown_restores(r):
    r = Rig(r.dir, cooldown_s=30)
    r.start()
    r.set(109)
    r.wait(lambda: 'enter' in r.verbs(), 'enter')
    r.proc.send_signal(signal.SIGTERM)
    r.proc.wait(10)
    assert r.verbs()[-1] == 'exit' and not (r.run / 'cooldown').exists()
    return r


def test_bad_config_keeps_the_protection(r):
    r = Rig(r.dir, emergency=90000, resume=95000)  # resume above emergency
    r.start()
    assert 'inconsistent thresholds' in r.log()
    r.set(100)
    time.sleep(0.4)
    assert 'prepare' not in r.verbs(), 'defaults (108) apply, not 90'
    r.set(109)
    r.wait(lambda: 'prepare' in r.verbs(), 'the default threshold')
    return r


def test_hung_action_is_killed(r):
    r = Rig(r.dir, action_timeout_s=1)
    (r.dir / 'hang').write_text('')
    r.start()
    r.set(109)
    r.wait(lambda: 'killed after 1 s' in r.log(), 'the action timeout', 8)
    (r.dir / 'hang').unlink()
    r.set(90)
    r.wait(lambda: 'exit' in r.verbs(), 'exit', 6)
    return r


def test_unreadable_sensor(r):
    r.start()
    r.sensor.write_text('garbage\n')
    time.sleep(0.4)
    assert r.proc.poll() is None and 'SENSOR unreadable' in r.log()
    assert 'prepare' not in r.verbs()
    r.set(109)
    r.wait(lambda: 'prepare' in r.verbs(), 'recovery of the sensor')


def test_dry_run(r):
    r.proc = subprocess.Popen([str(BIN), '-c', str(r.conf), '-n'], env={'PATH': os.environ['PATH']})
    r.wait(lambda: (r.run / 'state').exists(), 'state file')
    r.set(109)
    r.wait(lambda: 'ACTION enter  (dry run)' in r.log(), 'dry-run enter')
    assert r.verbs() == []


def main():
    global BIN
    with tempfile.TemporaryDirectory() as build:
        BIN = Path(build) / 'meow-thermald'
        subprocess.run(['cc', '-O2', '-Wall', '-Wextra', '-Werror', str(HERE / 'thermald.c'), '-o', str(BIN)], check=True)
        tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith('test_') and callable(f)]
        failed = 0
        for name, fn in tests:
            with tempfile.TemporaryDirectory() as tmp:
                rig = Rig(tmp)
                t0 = time.time()
                try:
                    rig = fn(rig) or rig
                    print(f'ok   {name} ({time.time() - t0:.1f} s)')
                except AssertionError as e:
                    failed += 1
                    print(f'FAIL {name}: {e}')
                finally:
                    rig.stop()
        print(f'{len(tests) - failed}/{len(tests)} passed')
        sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
