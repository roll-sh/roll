"""Deploys deploy/search/aws/roll-search.yaml to LocalStack (community edition) as far as it
implements the resources, and checks what it created. Run by tests/search/aws/localstack.sh.

  localstack.py <endpoint> <template> <supported types file> <out.json> <image uri> <proxy host>

The template is reduced, never edited by hand:
- resources whose type LocalStack has no CloudFormation provider for are dropped (printed: they
  are what only a real AWS account checks), and references to them become stand-ins; the load
  balancer's DNS name becomes `<proxy host>`, the proxy container the test starts, so the
  function's upstream reaches a real search-proxy;
- `!Select [i, !Cidr [...]]` is precomputed with AWS's definition (LocalStack 4.9 fails on it in
  both of its engines; real CloudFormation documents exactly this form);
- `Rules` are dropped (LocalStack doesn't evaluate them; cfn-lint and the model tests do).

Then the stack must reach CREATE_COMPLETE, and the resources must be configured as the template
says: VPC and subnets, security groups, the gateway endpoint, the bucket's encryption, public
access block and lifecycle, the secrets, IAM roles, log groups, the Lambda function (image,
EntryPoint and Command, arm64, timeouts, its environment with the TLS seed resolved from the
secret), its URL and permissions, and the CloudWatch alarms.
"""

import ipaddress
import json
import sys
import time

import boto3
import yaml

ENDPOINT, TEMPLATE, TYPES, OUT, IMAGE, PROXY_HOST = sys.argv[1:7]


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

with open(TEMPLATE) as f:
    doc = yaml.load(f, Loader=Loader)
with open(TYPES) as f:
    supported = {l.strip() for l in f if l.strip()}

dropped = {n: r["Type"] for n, r in doc["Resources"].items() if r["Type"] not in supported}
STAND_INS = {
    ("FrontDoor", "DNSName"): PROXY_HOST,
    ("FrontDoor", "LoadBalancerFullName"): "net/rehearsal/0123456789abcdef",
    ("Database", "Endpoint.Address"): "database.rehearsal.internal",
    ("Database", "Endpoint.Port"): "5432",
    ("NodeService", "Name"): "rehearsal-nodes",
    ("NodeLaunchTemplate", "LatestVersionNumber"): "1",
    ("WalExpress", "Arn"): "arn:aws:s3express:us-east-1:000000000000:bucket/rehearsal--use1-az4--x-s3",
}


# What `Ref` returns for the dropped types where the template needs a well-formed value.
REF_STAND_INS = {
    "AWS::ECS::Service": "arn:aws:ecs:us-east-1:000000000000:service/rehearsal-cluster/rehearsal-nodes",
    "AWS::ECS::Cluster": "rehearsal-cluster",
    "AWS::ApplicationAutoScaling::ScalingPolicy": "arn:aws:autoscaling:us-east-1:000000000000:scalingPolicy:"
    "00000000-0000-0000-0000-000000000000:resource/ecs/service/rehearsal-cluster/rehearsal-nodes:policyName/{name}",
}


def stand_in(name, attr=None):
    if attr is None and dropped[name] in REF_STAND_INS:
        return REF_STAND_INS[dropped[name]].format(name=name)
    return STAND_INS.get((name, attr), f"rehearsal-{name}" + (f"-{attr}" if attr else ""))


cidr = doc["Parameters"]["VpcCidr"]["Default"]


def reduce(v):
    if isinstance(v, dict) and len(v) == 1:
        (k, x), = v.items()
        if k == "Ref" and x in dropped:
            return stand_in(x)
        if k == "Fn::GetAtt" and x[0] in dropped:
            return stand_in(x[0], x[1])
        if k == "Fn::Select" and isinstance(x[1], dict) and "Fn::Cidr" in x[1]:
            block, count, bits = x[1]["Fn::Cidr"]
            assert block == {"Ref": "VpcCidr"}, block
            net = ipaddress.ip_network(cidr)
            subnets = list(net.subnets(new_prefix=32 - int(bits)))[: int(count)]
            return str(subnets[int(x[0])])
        if k == "Fn::Sub" and isinstance(x, str):
            for name in dropped:
                for (n, attr), val in STAND_INS.items():
                    if n == name:
                        x = x.replace(f"${{{n}.{attr}}}", val)
                x = x.replace(f"${{{name}}}", stand_in(name))
            return {k: x}
    if isinstance(v, dict):
        return {k: reduce(x) for k, x in v.items()}
    if isinstance(v, list):
        return [reduce(x) for x in v]
    return v


resources = {}
for name, r in doc["Resources"].items():
    if name in dropped:
        continue
    r = reduce(r)
    deps = r.get("DependsOn")
    if deps:
        deps = [d for d in ([deps] if isinstance(deps, str) else deps) if d not in dropped]
        if deps:
            r["DependsOn"] = deps
        else:
            del r["DependsOn"]
    resources[name] = r
reduced = {k: v for k, v in doc.items() if k not in ("Rules", "Metadata", "Resources", "Outputs")}
reduced["Resources"] = resources
reduced["Outputs"] = reduce(doc["Outputs"])

print("localstack: not implemented by LocalStack community (checked only on AWS):")
by_type = {}
for n, t in dropped.items():
    by_type.setdefault(t, []).append(n)
for t in sorted(by_type):
    print(f"  {t}: {', '.join(sorted(by_type[t]))}")

session = boto3.session.Session(aws_access_key_id="test", aws_secret_access_key="test", region_name="us-east-1")
client = lambda s: session.client(s, endpoint_url=ENDPOINT)  # noqa: E731
cfn, ec2, s3, sm, iam, lam, cw, logs, ssm = (client(s) for s in ("cloudformation", "ec2", "s3", "secretsmanager", "iam", "lambda", "cloudwatch", "logs", "ssm"))

ssm.put_parameter(Name="/roll/rehearsal/ecs-ami", Type="String", Value="ami-0123456789abcdef0", Overwrite=True)
params = {"ImageUri": IMAGE, "EcsAmiId": "/roll/rehearsal/ecs-ami", "MinNodes": "0"}
stack = "roll-search-rehearsal"
cfn.create_stack(StackName=stack, TemplateBody=json.dumps(reduced), Capabilities=["CAPABILITY_IAM"],
                 Parameters=[{"ParameterKey": k, "ParameterValue": v} for k, v in params.items()])
deadline = time.time() + 600
time.sleep(5)
while True:
    s = cfn.describe_stacks(StackName=stack)["Stacks"][0]
    if s["StackStatus"] in ("CREATE_COMPLETE", "CREATE_FAILED", "ROLLBACK_COMPLETE", "ROLLBACK_FAILED"):
        break
    if time.time() > deadline:
        raise SystemExit("localstack: the stack never finished")
    time.sleep(2)
if s["StackStatus"] != "CREATE_COMPLETE":
    events = cfn.describe_stack_events(StackName=stack)["StackEvents"]
    for e in events:
        if "FAILED" in e["ResourceStatus"]:
            print(f"  {e['LogicalResourceId']} {e['ResourceType']}: {e.get('ResourceStatusReason')}")
    raise SystemExit(f"localstack: FAILED: stack {s['StackStatus']}")
phys = {r["LogicalResourceId"]: r.get("PhysicalResourceId") for r in cfn.describe_stack_resources(StackName=stack)["StackResources"]}
missing = sorted(n for n, r in resources.items() if not phys.get(n) and ("Condition" not in r or r["Condition"] != "UseExpress"))
if missing:
    print(f"localstack: resources without a physical id: {missing}")
outputs = {o["OutputKey"]: o["OutputValue"] for o in s.get("Outputs", [])}
print(f"localstack: stack CREATE_COMPLETE with {len(phys)} resources")


def check(cond, what):
    if not cond:
        raise SystemExit(f"localstack: FAILED: {what}")


# Network.
vpc = ec2.describe_vpcs(VpcIds=[phys["Vpc"]])["Vpcs"][0]
check(vpc["CidrBlock"] == cidr, f"VPC CIDR {vpc['CidrBlock']}")
subnets = ec2.describe_subnets(SubnetIds=[phys[n] for n in ("SubnetA", "SubnetB", "SubnetC")])["Subnets"]
check(sorted(s["CidrBlock"] for s in subnets) == sorted(str(n) for n in list(ipaddress.ip_network(cidr).subnets(new_prefix=20))[:3]), f"subnets {[s['CidrBlock'] for s in subnets]}")
check(len({s["AvailabilityZone"] for s in subnets}) == 3, f"subnets share an AZ: {[s['AvailabilityZone'] for s in subnets]}")
check(not any(s.get("MapPublicIpOnLaunch") for s in subnets), "subnets map public IPs")
# LocalStack 4.9 creates the groups but applies none of their rules (model.py checks those).
sgs = ec2.describe_security_groups(GroupIds=[phys[n] for n in ("FrontSecurityGroup", "ProxySecurityGroup", "NodeSecurityGroup", "DatabaseSecurityGroup", "LambdaSecurityGroup")])["SecurityGroups"]
check(len(sgs) == 5 and all(g["VpcId"] == phys["Vpc"] for g in sgs), f"security groups {sgs}")
endpoints = ec2.describe_vpc_endpoints(VpcEndpointIds=[phys["S3Endpoint"]])["VpcEndpoints"]
check(endpoints[0]["ServiceName"] == "com.amazonaws.us-east-1.s3" and endpoints[0]["VpcEndpointType"] == "Gateway", f"S3 endpoint {endpoints}")
print("localstack: VPC, three subnets in three AZs, security groups (rules not applied by LocalStack), S3 gateway endpoint: ok")

# Storage.
bucket = phys["Data"]
enc = s3.get_bucket_encryption(Bucket=bucket)["ServerSideEncryptionConfiguration"]["Rules"][0]
check(enc["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256", f"bucket encryption {enc}")
pab = s3.get_public_access_block(Bucket=bucket)["PublicAccessBlockConfiguration"]
check(all(pab[k] for k in ("BlockPublicAcls", "BlockPublicPolicy", "IgnorePublicAcls", "RestrictPublicBuckets")), f"public access block {pab}")
try:
    rules = s3.get_bucket_lifecycle_configuration(Bucket=bucket)["Rules"]
    check(rules[0]["AbortIncompleteMultipartUpload"]["DaysAfterInitiation"] == 1, f"lifecycle {rules}")
except s3.exceptions.ClientError as e:
    # LocalStack 4.9's bucket provider drops a lifecycle rule with no expiration.
    check("NoSuchLifecycleConfiguration" in str(e), str(e))
    print("localstack: the bucket's lifecycle rule isn't applied by LocalStack (checked only on AWS)")
own = s3.get_bucket_ownership_controls(Bucket=bucket)["OwnershipControls"]["Rules"][0]["ObjectOwnership"]
check(own == "BucketOwnerEnforced", f"ownership {own}")
print("localstack: bucket (AES256, public access blocked, owner enforced): ok")

# Secrets.
token = sm.get_secret_value(SecretId=phys["ApiToken"])["SecretString"]
seed = sm.get_secret_value(SecretId=phys["ProxyTlsSeed"])["SecretString"]
dbpw = json.loads(sm.get_secret_value(SecretId=phys["DatabasePassword"])["SecretString"])
check(len(token) == 48 and token.isalnum(), f"API token {len(token)} chars")
check(len(seed) == 48 and seed.isalnum(), f"TLS seed {len(seed)} chars")
check(dbpw["username"] == "roll" and len(dbpw["password"]) == 40 and dbpw["password"].isalnum(), "database password")
print("localstack: secrets (API token, TLS seed, database password): ok")

# IAM.
role = iam.get_role(RoleName=phys["ExecutionRole"])["Role"]
check(role["AssumeRolePolicyDocument"]["Statement"][0]["Principal"]["Service"] == "ecs-tasks.amazonaws.com", "execution role trust")
pol = iam.get_role_policy(RoleName=phys["ExecutionRole"], PolicyName="secrets")["PolicyDocument"]["Statement"][0]
check(set(pol["Resource"]) == {phys["ApiToken"], phys["DatabasePassword"], phys["ProxyTlsSeed"]}, f"execution role secrets {pol['Resource']}")
data = iam.get_role_policy(RoleName=phys["NodeTaskRole"], PolicyName="data")["PolicyDocument"]["Statement"]
check(data[0]["Resource"] == f"arn:aws:s3:::{bucket}/*" and "s3:PutObject" in data[0]["Action"], f"node data policy {data}")
wake = iam.get_role_policy(RoleName=phys["ProxyTaskRole"], PolicyName="wake")["PolicyDocument"]["Statement"][0]
check(sorted(wake["Action"]) == ["ecs:DescribeServices", "ecs:UpdateService"], f"proxy wake policy {wake}")
print("localstack: IAM roles (secrets, bucket, wake): ok")

# The Lambda front door.
fn = lam.get_function(FunctionName=phys["ProxyFunction"])["Configuration"]
check(fn["PackageType"] == "Image" and fn["Architectures"] == ["arm64"], f"function {fn['PackageType']} {fn['Architectures']}")
check(fn["ImageConfigResponse"]["ImageConfig"] == {"EntryPoint": ["/usr/local/bin/search-proxy"], "Command": ["--lambda"]}, f"image config {fn['ImageConfigResponse']}")
check(fn["Timeout"] == 150 and fn["MemorySize"] == 256, f"timeout {fn['Timeout']} memory {fn['MemorySize']}")
env = fn["Environment"]["Variables"]
check(env["ROLL_SEARCH_LAMBDA_TLS_SEED"] == seed, "the function's TLS seed is not the secret's value")
check(env["ROLL_SEARCH_LAMBDA_UPSTREAM"] == f"https://{PROXY_HOST}:9200" and env["ROLL_SEARCH_LAMBDA_API_UPSTREAM"] == f"https://{PROXY_HOST}:7070", f"upstreams {env}")
url = lam.get_function_url_config(FunctionName=phys["ProxyFunction"])
check(url["AuthType"] == "NONE" and url["InvokeMode"] == "BUFFERED", f"function URL {url}")
policy = json.loads(lam.get_policy(FunctionName=phys["ProxyFunction"])["Policy"])["Statement"]
check(any(s["Action"] == "lambda:InvokeFunctionUrl" for s in policy), f"function policy {policy}")
check(outputs["Endpoint"] == url["FunctionUrl"] and outputs["ApiUrl"] == url["FunctionUrl"], f"outputs {outputs}")
groups = {g["logGroupName"]: g for g in logs.describe_log_groups()["logGroups"]}
# (LocalStack 4.9 creates log groups without their retention.)
for g in (f"/aws/lambda/{stack}-front", f"/ecs/{stack}"):
    check(g in groups and groups[g].get("retentionInDays", 14) == 14, f"log group {g}: {groups.get(g)}")
print("localstack: Lambda function (image, EntryPoint + Command [--lambda], arm64, seed resolved), URL, permissions, logs: ok")

# Alarms (MinNodes 0: the scale-in and idle alarms).
alarms = {a["AlarmName"]: a for a in cw.describe_alarms()["MetricAlarms"]}
idle = alarms[phys["IdleAlarm"]]
check(idle["Threshold"] == 0.5 and idle["ComparisonOperator"] == "GreaterThanOrEqualToThreshold" and idle["EvaluationPeriods"] == 15, f"idle alarm {idle}")
check(any(m.get("Expression") == "IF(FILL(bytes, 0) == 0 AND FILL(tasks, 0) > 0, 1, 0)" for m in idle["Metrics"]), f"idle alarm metrics {idle['Metrics']}")
shrink = alarms[phys["ScaleInAlarm"]]
check(any(m.get("Expression") == "IF(cpu < 60 / 3 AND tasks > 1, 1, 0)" for m in shrink["Metrics"]), f"scale-in alarm metrics {shrink['Metrics']}")
print("localstack: CloudWatch alarms (idle and scale-in metric math): ok")

with open(OUT, "w") as f:
    json.dump({"function_url": url["FunctionUrl"], "token": token, "seed": seed, "dropped": by_type,
               "function": {"entrypoint": fn["ImageConfigResponse"]["ImageConfig"]["EntryPoint"],
                            "command": fn["ImageConfigResponse"]["ImageConfig"]["Command"],
                            "environment": env, "timeout": fn["Timeout"], "memory": fn["MemorySize"],
                            "image": IMAGE}}, f)
