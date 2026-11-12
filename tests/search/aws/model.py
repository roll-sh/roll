"""The roll search (and roll queues) CloudFormation templates' scaling and capacity rules,
evaluated with AWS's documented semantics, for what LocalStack can't run.

Run by scripts/search-aws-rehearsal.sh model (needs PyYAML and dash):

    python3 -I tests/search/aws/model.py deploy/search/aws/roll-search.yaml

What is modelled, and from which AWS documentation:
- Step adjustments (Application Auto Scaling, "Step adjustments"): bounds are relative to the
  alarm threshold; above the threshold the lower bound is inclusive and the upper exclusive,
  below it the reverse. A breach exactly at the threshold is neither, so a policy must never
  depend on it.
- Alarm evaluation (CloudWatch, "Evaluating an alarm"): the newest EvaluationPeriods datapoints,
  ALARM when DatapointsToAlarm of them breach. An alarm in ALARM keeps invoking its scaling
  policy every minute ("the adjustments can continue for additional alarm breaches until the
  alarm state returns to OK"), and a scale-in is blocked for the policy's Cooldown (default
  300 s) after the last one.
- Metric math: the IF/FILL/AND/comparison subset the templates use, evaluated per minute.
- Metrics arrive late: a minute's datapoint is visible LAG minutes after it.
- ECS: a desired count change starts tasks after a boot delay; a stopping task drains for up to
  two minutes and keeps publishing CPU while it runs. The proxies wake the service while they
  hold a request and no node runs.
- Deploys (ECS deployment configuration + distinctInstance + capacity provider managed scaling):
  MaximumPercent of the desired count runs at once, one task per instance, and the Auto Scaling
  group never exceeds its MaxSize.
"""

import math
import re
import subprocess
import sys
import unittest

import yaml

LAG = 2  # minutes until a minute's datapoint is visible to its alarm (NLB metrics: 1-3)
QUEUES = "deploy/queues/aws/roll-queues.yaml"


# ── template loading ──

class Loader(yaml.SafeLoader):
    pass


def _tag(loader, suffix, node):
    name = {"Ref": "Ref", "Condition": "Condition"}.get(suffix, f"Fn::{suffix}")
    if isinstance(node, yaml.ScalarNode):
        value = loader.construct_scalar(node)
        if suffix == "GetAtt":
            value = value.split(".", 1)
    elif isinstance(node, yaml.SequenceNode):
        value = loader.construct_sequence(node, deep=True)
    else:
        value = loader.construct_mapping(node, deep=True)
    return {name: value}


Loader.add_multi_constructor("!", _tag)


class Template:
    def __init__(self, path, **params):
        with open(path) as f:
            self.doc = yaml.load(f, Loader=Loader)
        self.params = {k: v.get("Default") for k, v in self.doc["Parameters"].items()}
        self.params.update(params)
        self.resources = self.doc["Resources"]

    def condition(self, name):
        return self.cond(self.doc["Conditions"][name])

    def cond(self, c):
        (k, v), = c.items()
        if k == "Fn::Equals":
            return str(self.value(v[0])) == str(self.value(v[1]))
        if k == "Fn::Not":
            return not self.cond(v[0])
        if k == "Fn::And":
            return all(self.cond(x) for x in v)
        if k == "Fn::Or":
            return any(self.cond(x) for x in v)
        if k == "Condition":
            return self.condition(v)
        raise ValueError(c)

    def value(self, v):
        """Resolves parameters and conditions; other intrinsics stay as they are."""
        if isinstance(v, dict) and len(v) == 1:
            (k, x), = v.items()
            if k == "Ref" and x in self.params:
                return self.params[x]
            if k == "Ref" and x == "AWS::NoValue":
                return None
            if k == "Fn::If":
                return self.value(x[1] if self.condition(x[0]) else x[2])
            if k == "Fn::Sub" and isinstance(x, str):
                return re.sub(r"\$\{([A-Za-z0-9]+)\}", lambda m: str(self.params.get(m.group(1), m.group(0))), x)
        if isinstance(v, dict):
            return {k: self.value(x) for k, x in v.items()}
        if isinstance(v, list):
            return [y for y in (self.value(x) for x in v) if y is not None]
        return v

    def props(self, name):
        r = self.resources[name]
        if "Condition" in r and not self.condition(r["Condition"]):
            return None
        return self.value(r["Properties"])


# ── metric math: the subset the templates use ──

TOKEN = re.compile(r"\s*(?:(\d+(?:\.\d+)?)|([A-Za-z_][A-Za-z0-9_]*)|(==|!=|>=|<=|[-+*/()<>,]))")
PREC = {"OR": 1, "AND": 2, "==": 3, "!=": 3, ">": 3, "<": 3, ">=": 3, "<=": 3, "+": 4, "-": 4, "*": 5, "/": 5}


def parse(expr):
    toks = []
    pos = 0
    while expr[pos:].strip():
        m = TOKEN.match(expr, pos)
        if not m:
            raise ValueError(f"can't parse {expr[pos:]!r}")
        num, ident, op = m.groups()
        toks.append(("num", float(num)) if num else ("id", ident) if ident else ("op", op))
        pos = m.end()
    toks.append(("end", None))
    i = 0

    def take(want=None):
        nonlocal i
        t = toks[i]
        if want and t != want:
            raise ValueError(f"expected {want}, got {t} in {expr!r}")
        i += 1
        return t

    def atom():
        t = take()
        if t[0] == "num":
            return ("num", t[1])
        if t == ("op", "("):
            e = binary(1)
            take(("op", ")"))
            return e
        if t[0] == "id":
            if toks[i] == ("op", "("):
                take()
                args = [binary(1)]
                while toks[i] == ("op", ","):
                    take()
                    args.append(binary(1))
                take(("op", ")"))
                return ("call", t[1], args)
            return ("var", t[1])
        raise ValueError(f"unexpected {t} in {expr!r}")

    def binary(min_prec):
        left = atom()
        while True:
            p = PREC.get(toks[i][1]) if toks[i][0] in ("op", "id") else None
            if p is None or p < min_prec:
                return left
            op = take()[1]
            left = ("bin", op, left, binary(p + 1))

    e = binary(1)
    take(("end", None))
    return e


def evaluate(e, env):
    """None is a missing datapoint: comparisons with it are false, FILL replaces it."""
    kind = e[0]
    if kind == "num":
        return e[1]
    if kind == "var":
        return env.get(e[1])
    if kind == "call":
        name, args = e[1], e[2]
        if name == "IF":
            return evaluate(args[1] if evaluate(args[0], env) else args[2], env)
        if name == "FILL":
            v = evaluate(args[0], env)
            return evaluate(args[1], env) if v is None else v
        raise ValueError(f"function {name}")
    op, a, b = e[1], evaluate(e[2], env), evaluate(e[3], env)
    if op == "AND":
        return float(bool(a) and bool(b))
    if op == "OR":
        return float(bool(a) or bool(b))
    if a is None or b is None:
        return None if op in "+-*/" else 0.0
    return {
        "==": lambda: float(a == b), "!=": lambda: float(a != b), ">": lambda: float(a > b), "<": lambda: float(a < b),
        ">=": lambda: float(a >= b), "<=": lambda: float(a <= b), "+": lambda: a + b, "-": lambda: a - b,
        "*": lambda: a * b, "/": lambda: a / b,
    }[op]()


# ── alarms and step policies ──

def breaches(alarm, value):
    t = float(alarm["Threshold"])
    if value is None:
        return alarm.get("TreatMissingData", "missing") == "breaching"
    return {
        "GreaterThanOrEqualToThreshold": value >= t, "GreaterThanThreshold": value > t,
        "LessThanOrEqualToThreshold": value <= t, "LessThanThreshold": value < t,
    }[alarm["ComparisonOperator"]]


def step(policy, alarm, value):
    """The step adjustment AWS applies for a breaching value, or None (no step matches)."""
    d = value - float(alarm["Threshold"])
    for s in policy["StepScalingPolicyConfiguration"]["StepAdjustments"]:
        lo, hi = s.get("MetricIntervalLowerBound"), s.get("MetricIntervalUpperBound")
        lo = -math.inf if lo is None else float(lo)
        hi = math.inf if hi is None else float(hi)
        if (d > 0 and lo <= d < hi) or (d < 0 and lo < d <= hi):
            return s["ScalingAdjustment"]
        # d == 0: AWS documents neither case; a template relying on it is wrong.
    return None


def alarm_value(alarm, sample):
    """The alarm's value for one minute's datapoints (`sample`: metric id -> value or None)."""
    env = dict(sample)
    out = None
    for m in alarm["Metrics"]:
        if "Expression" in m:
            env[m["Id"]] = evaluate(parse(m["Expression"]), env)
        if m.get("ReturnData", True):
            out = env.get(m["Id"])
    return out


class Fleet:
    """Minute by minute: an ECS service's desired count and tasks, the load balancer's traffic,
    one alarm with its step policy."""

    def __init__(self, alarm, policy, ids, boot=1, drain=2):
        self.alarm, self.policy, self.ids = alarm, policy, ids
        self.boot, self.drain = boot, drain
        self.desired = 1
        self.tasks = [["running", 0]]  # [state, since minute]
        self.samples = []
        self.last_scale_in = None
        self.cooldown = int(policy["StepScalingPolicyConfiguration"].get("Cooldown", 300)) // 60
        self.log = []  # (minute, who, old desired, new desired)

    def serving(self):
        return any(s == "running" for s, _ in self.tasks)

    def set_desired(self, n, t, who):
        if n == self.desired:
            return
        self.log.append((t, who, self.desired, n))
        for _ in range(self.desired - n):
            # ECS stops a task still starting at once, else drains a running one.
            starting = [x for x in self.tasks if x[0] == "starting"]
            if starting:
                self.tasks.remove(starting[-1])
            else:
                next(x for x in reversed(self.tasks) if x[0] == "running")[:] = ["stopping", t]
        for _ in range(n - self.desired):
            self.tasks.append(["starting", t])
        self.desired = n

    def minute(self, t, traffic, wake):
        for x in self.tasks:
            if x[0] == "starting" and t - x[1] >= self.boot:
                x[:] = ["running", t]
        self.tasks = [x for x in self.tasks if not (x[0] == "stopping" and t - x[1] >= self.drain)]
        if wake and not self.serving() and self.desired < 1:
            self.set_desired(1, t, "proxy wake")
        n = len(self.tasks) - sum(1 for s, _ in self.tasks if s == "starting")
        # Nothing is published for a minute without traffic, or without running tasks.
        self.samples.append({self.ids["traffic"]: 1000.0 if traffic else None, self.ids["tasks"]: float(n) if n else None})
        visible = self.samples[: max(0, t - LAG + 1)]
        periods = int(self.alarm["EvaluationPeriods"])
        need = int(self.alarm.get("DatapointsToAlarm", periods))
        values = [alarm_value(self.alarm, s) for s in visible[-periods:]]
        if len(values) < periods or sum(1 for v in values if breaches(self.alarm, v)) < need:
            return
        adj = step(self.policy, self.alarm, values[-1])
        assert adj is not None, f"no step adjustment matches the breach {values[-1]} (threshold {self.alarm['Threshold']})"
        target = adj if self.policy["StepScalingPolicyConfiguration"]["AdjustmentType"] == "ExactCapacity" else self.desired + adj
        if target < self.desired and (self.last_scale_in is None or t - self.last_scale_in >= self.cooldown):
            self.set_desired(max(0, target), t, "alarm")
            self.last_scale_in = t


def simulate(policy, alarm, ids, idle_after, wake_at, minutes=200, boot=1):
    """Traffic until `idle_after`, then nothing but a request at each minute in `wake_at`: it
    passes the load balancer when it arrives and the proxy holds it, waking the fleet, until a
    node runs. Returns the fleet and (arrived, answered) minutes."""
    f = Fleet(alarm, policy, ids, boot=boot)
    held = None
    answered = []
    for t in range(minutes):
        if t in wake_at:
            held = t
        f.minute(t, traffic=t < idle_after or t in wake_at, wake=held is not None)
        if held is not None and f.serving():
            answered.append((held, t))
            held = None
    return f, answered


def to_zero_parts(tpl, policy_name="ToZeroPolicy", alarm_name="IdleAlarm"):
    policy, alarm = tpl.props(policy_name), tpl.props(alarm_name)
    ids = {}
    for m in alarm["Metrics"]:
        if "MetricStat" in m:
            ids["traffic" if m["MetricStat"]["Metric"]["Namespace"] == "AWS/NetworkELB" else "tasks"] = m["Id"]
    return policy, alarm, ids


# ── tests ──

class Templates(unittest.TestCase):
    path = "deploy/search/aws/roll-search.yaml"

    def tpl(self, **p):
        return Template(self.path, **p)

    def test_every_step_policy_acts_on_its_alarm(self):
        """Every alarm's breach (its 0/1 expression at 1) lands strictly inside a step."""
        for tpl in (self.tpl(MinNodes=0), Template(QUEUES, IdleMinutesToZero=15)):
            for p, a, want in [("ScaleInPolicy", "ScaleInAlarm", -1), ("ToZeroPolicy", "IdleAlarm", 0)]:
                policy, alarm = tpl.props(p), tpl.props(a)
                self.assertNotEqual(float(alarm["Threshold"]), 1.0, f"{a}: a breach exactly at the threshold has no documented step")
                self.assertEqual(step(policy, alarm, 1.0), want, f"{p} on {a}")
                self.assertTrue(breaches(alarm, 1.0) and not breaches(alarm, 0.0), a)

    def test_an_upper_bound_of_zero_never_acts(self):
        """The queues template's old step: [{ MetricIntervalUpperBound: 0 }] on an alarm that
        breaches above its threshold matches nothing."""
        alarm = {"Threshold": 0.5, "ComparisonOperator": "GreaterThanOrEqualToThreshold"}
        policy = {"StepScalingPolicyConfiguration": {"StepAdjustments": [{"MetricIntervalUpperBound": 0, "ScalingAdjustment": 0}]}}
        self.assertIsNone(step(policy, alarm, 1.0))

    def scale_to_zero(self, tpl, idle_minutes):
        p, a, ids = to_zero_parts(tpl)
        # Idle: the fleet stops within the idle period, the metric lag and a minute.
        f, _ = simulate(p, a, ids, idle_after=10, wake_at=[])
        stops = [t for t, who, _, n in f.log if who == "alarm" and n == 0]
        self.assertTrue(stops, f"never scaled to zero: {f.log}")
        self.assertLessEqual(stops[0], 10 + idle_minutes + LAG + 1, f.log)
        self.assertEqual(f.desired, 0)
        # A request at any minute after the fleet went idle (before, during and after the stop)
        # answers within two minutes. A request the alarm hasn't seen yet (the metric lag) may
        # still meet the stop, which then hands its cluster over; but a wake (the proxy raising
        # the desired count) is never undone while the woken node has had traffic.
        for w in range(11, 10 + idle_minutes + LAG + 20):
            f, answered = simulate(p, a, ids, idle_after=10, wake_at=[w])
            self.assertTrue(answered and answered[0][0] == w, f"wake at minute {w} never answered: {f.log}")
            self.assertLessEqual(answered[0][1] - w, 2, f"wake at minute {w} took {answered[0][1] - w} minutes: {f.log}")
            woke = [t for t, who, _, _ in f.log if who == "proxy wake"]
            kills = [t for t, who, old, n in f.log if who == "alarm" and n < old and woke and woke[0] <= t < woke[0] + idle_minutes]
            self.assertFalse(kills, f"wake at minute {w} undone by the alarm at minute {kills}: {f.log}")
            # …and the fleet still stops once idle again.
            self.assertEqual(f.desired, 0, f"wake at minute {w}: never stopped again: {f.log}")

    def lags(self, run):
        # Load balancer metrics show up one to three minutes late.
        global LAG
        try:
            for LAG in (1, 2, 3):
                run()
        finally:
            LAG = 2

    def test_scale_to_zero_and_wake(self):
        for idle in (15, 1):
            self.lags(lambda: self.scale_to_zero(self.tpl(MinNodes=0, IdleMinutesToZero=idle), idle))
        self.assertEqual(self.tpl(MinNodes=0).props("NodeScaling")["MinCapacity"], 0)

    def test_queues_scale_to_zero_and_wake(self):
        for idle in (15, 1):
            self.lags(lambda: self.scale_to_zero(Template(QUEUES, IdleMinutesToZero=idle), idle))

    def test_the_to_zero_cooldown_is_what_keeps_a_wake(self):
        """An alarm still in ALARM after the fleet stopped (the drain, then the metric lag)
        re-runs its policy every minute; only the scale-in cooldown keeps it from stopping the
        node a wake starts meanwhile. The templates set it explicitly."""
        for tpl in (self.tpl(MinNodes=0), Template(QUEUES, IdleMinutesToZero=15)):
            p, a, ids = to_zero_parts(tpl)
            self.assertGreaterEqual(int(p["StepScalingPolicyConfiguration"]["Cooldown"]), 600)
            p["StepScalingPolicyConfiguration"]["Cooldown"] = 60
            undone = []
            for w in range(11, 60):
                f, _ = simulate(p, a, ids, idle_after=10, wake_at=[w])
                woke = [t for t, who, _, _ in f.log if who == "proxy wake"]
                if any(who == "alarm" and n < o and woke and woke[0] <= t for t, who, o, n in f.log):
                    undone.append(w)
            self.assertTrue(undone, "without the cooldown a wake right after the stop is undone")

    def test_the_old_queues_idle_alarm(self):
        """The simulator has teeth: the queues alarm before this round (ActiveFlowCount <= 0,
        missing data breaching, no running-task term) never matched its step, and with the step
        fixed it stopped brokers a wake had just started."""
        tpl = Template(QUEUES, IdleMinutesToZero=15)
        policy = tpl.props("ToZeroPolicy")
        old = {"Threshold": 0, "ComparisonOperator": "LessThanOrEqualToThreshold", "TreatMissingData": "breaching",
               "EvaluationPeriods": 15, "Metrics": [{"Id": "flows", "ReturnData": False, "MetricStat": {"Metric": {"Namespace": "AWS/NetworkELB"}}},
                                                    {"Id": "tasks", "ReturnData": False, "MetricStat": {"Metric": {"Namespace": "AWS/ECS"}}},
                                                    {"Id": "out", "Expression": "FILL(flows, 0)"}]}
        ids = {"traffic": "flows", "tasks": "tasks"}
        policy["StepScalingPolicyConfiguration"]["StepAdjustments"] = [{"MetricIntervalUpperBound": 0, "ScalingAdjustment": 0}]
        with self.assertRaisesRegex(AssertionError, "no step adjustment matches"):
            simulate(policy, old, ids, idle_after=10, wake_at=[])
        old["Threshold"] = 0.5
        undone = []
        for w in range(30, 80):
            f, _ = simulate(policy, old, ids, idle_after=10, wake_at=[w])
            if any(who == "alarm" and n < o and t >= w for t, who, o, n in f.log if t < w + 15):
                undone.append(w)
        self.assertTrue(undone, "the old alarm should undo wakes while the fleet is at zero")

    def test_scale_in_one_node_at_a_time_never_the_last(self):
        tpl = self.tpl()
        p, a = tpl.props("ScaleInPolicy"), tpl.props("ScaleInAlarm")
        self.assertEqual(p["StepScalingPolicyConfiguration"]["AdjustmentType"], "ChangeInCapacity")
        low = float(tpl.params["CpuTarget"]) / 3
        for tasks, cpu, want in [(3, 5.0, 1.0), (1, 5.0, 0.0), (3, low + 1, 0.0), (2, 0.0, 1.0)]:
            self.assertEqual(alarm_value(a, {"cpu": cpu, "tasks": float(tasks)}), want, (tasks, cpu))
        self.assertEqual(step(p, a, 1.0), -1)
        self.assertGreaterEqual(int(p["StepScalingPolicyConfiguration"]["Cooldown"]), 300)

    def test_deploys_have_room_for_a_new_node_beside_each_old_one(self):
        """distinctInstance + MaximumPercent 200 + MinimumHealthyPercent 100: a deploy at full
        size needs 2 x MaxNodes instances, which managed scaling can only launch below the
        group's MaxSize (else the new tasks stay PROVISIONING and the circuit breaker rolls the
        deploy back)."""
        for max_nodes in (1, 2, 8, int(self.tpl().doc["Parameters"]["MaxNodes"]["MaxValue"])):
            tpl = self.tpl(MaxNodes=max_nodes)
            svc, asg = tpl.props("NodeService"), tpl.props("NodeGroup")
            dc = svc["DeploymentConfiguration"]
            self.assertEqual(svc["PlacementConstraints"], [{"Type": "distinctInstance"}])
            desired = int(tpl.props("NodeScaling")["MaxCapacity"])
            during = desired * int(dc["MaximumPercent"]) // 100
            self.assertGreaterEqual(math.ceil(desired * int(dc["MinimumHealthyPercent"]) / 100), desired, "old nodes stay until new ones run")
            self.assertGreater(during, desired, "a deploy must start new nodes first")
            self.assertLessEqual(during, int(asg["MaxSize"]), f"MaxNodes {max_nodes}: a deploy needs {during} instances, MaxSize is {asg['MaxSize']}")

    def test_queues_deploys_have_room_for_a_new_broker_beside_each_old_one(self):
        """The queues brokers have no distinctInstance, but an instance of the template's
        default size holds one (ECS registers less memory than the instance has, and two
        BrokerMemory tasks need all of it): a deploy at full size needs 2 x MaxBrokers."""
        tpl = Template(QUEUES)
        self.assertGreater(2 * int(tpl.params["BrokerMemory"]), int(tpl.params["BrokerInstanceMemoryMiB"]) - 256, "one broker per default instance")
        for max_brokers in (1, 2, 12, int(tpl.doc["Parameters"]["MaxBrokers"]["MaxValue"])):
            tpl = Template(QUEUES, MaxBrokers=max_brokers)
            svc, asg = tpl.props("BrokerService"), tpl.props("BrokerGroup")
            desired = int(tpl.props("BrokerScaling")["MaxCapacity"])
            during = desired * int(svc["DeploymentConfiguration"]["MaximumPercent"]) // 100
            self.assertGreater(during, desired)
            self.assertLessEqual(during, int(asg["MaxSize"]), f"MaxBrokers {max_brokers}: a deploy needs {during} instances, MaxSize is {asg['MaxSize']}")

    def test_node_cache_and_memory_from_the_task_command(self):
        """The node task's own shell arithmetic, run by dash (the image's /bin/sh)."""
        tpl = self.tpl()
        node = tpl.props("NodeTask")["ContainerDefinitions"][0]
        self.assertEqual(node["Command"][:2], ["sh", "-c"])
        script = node["Command"][2].replace("rm -rf /data/search/cache/wal;", "")
        script = script.replace("exec rolld", 'echo "$ROLL_SEARCH_CACHE_BYTES $ROLL_SEARCH_MEMORY_MB"')
        p = tpl.doc["Parameters"]["NodeCacheGiB"]
        for gib in (int(p["MinValue"]), 30, 64, int(p["MaxValue"])):
            out = subprocess.run(["dash", "-c", script], env={"ROLL_SEARCH_CACHE_GIB": str(gib), "ROLL_SEARCH_TASK_MEMORY_MB": "6144"},
                                 capture_output=True, text=True, check=True).stdout.split()
            self.assertEqual(int(out[0]), (gib - max(gib // 10, 2)) << 30, gib)
            self.assertGreaterEqual((gib << 30) - int(out[0]), 2 << 30)
            self.assertEqual(int(out[1]), 6144 * 3 // 4)

    def test_stop_and_hold_budgets(self):
        tpl = self.tpl()
        node = tpl.props("NodeTask")["ContainerDefinitions"][0]
        env = {e["Name"]: e["Value"] for e in node["Environment"]}
        # ECS sends SIGKILL StopTimeout after SIGTERM; Spot gives two minutes from the notice.
        self.assertLess(int(env["ROLL_SEARCH_SHUTDOWN_TIMEOUT"]), int(node["StopTimeout"]))
        self.assertLessEqual(int(node["StopTimeout"]), 120)
        ud = tpl.resources["NodeLaunchTemplate"]["Properties"]["LaunchTemplateData"]["UserData"]["Fn::Base64"]["Fn::Sub"]
        self.assertIn("ECS_ENABLE_SPOT_INSTANCE_DRAINING=true", ud)
        proxy = tpl.props("ProxyTask")["ContainerDefinitions"][0]
        penv = {e["Name"]: e["Value"] for e in proxy["Environment"]}
        self.assertLess(int(penv["ROLL_SEARCH_PROXY_DRAIN"]), int(proxy["StopTimeout"]))
        fn = tpl.props("ProxyFunction")
        self.assertLess(int(penv["ROLL_SEARCH_PROXY_HOLD_TIMEOUT"]), int(fn["Timeout"]), "a held request must answer before the function times out")
        self.assertLessEqual(int(fn["Timeout"]), 900)
        self.assertEqual(fn["ImageConfig"], {"EntryPoint": ["/usr/local/bin/search-proxy"], "Command": ["--lambda"]})
        self.assertEqual(fn["Architectures"], ["arm64"])
        # A node task of the default size fits an instance of the default size beside the agent.
        self.assertLessEqual(int(tpl.params["NodeMemory"]), int(tpl.params["NodeInstanceMemoryMiB"]) * 0.9)
        self.assertLessEqual(int(tpl.params["NodeCpu"]), int(tpl.params["NodeVcpus"]) * 1024)

    def test_nothing_reaches_nodes_or_the_database_from_outside(self):
        tpl = self.tpl()
        for sg in ("NodeSecurityGroup", "DatabaseSecurityGroup"):
            for rule in tpl.props(sg).get("SecurityGroupIngress", []):
                self.assertIn("SourceSecurityGroupId", rule, f"{sg} admits {rule}")
        self.assertIs(tpl.props("Database")["PubliclyAccessible"], False)
        for name in ("NodeTask", "ProxyTask"):
            for c in tpl.props(name)["ContainerDefinitions"]:
                for e in c.get("Environment", []):
                    self.assertNotRegex(e["Name"], "PASSWORD|TOKEN|SEED", f"{name}: secrets belong in Secrets")


if __name__ == "__main__":
    if len(sys.argv) > 1:
        Templates.path = sys.argv.pop(1)
    unittest.main(verbosity=2)
