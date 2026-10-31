"""Gitea against the server: its own database (xorm migrations and sync), then its REST API's
objects, searches, errors and deletes. See gitea.sh."""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402
import psycopg  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52455"))
PGHOST, PGPORT = os.environ["PGHOST"], os.environ["PGPORT"]
BASE = "http://%s:%d" % (PGHOST, PORT)
PW = "Adm1n-pass-133"
g = apps.Http(BASE + "/api/v1", "admin", PW)
# Commit ids, urls and per-run values; what's left is what Gitea computed from its database.
DROP = apps.VOLATILE | {
    "sha", "html_url", "clone_url", "ssh_url", "original_url", "avatar_url", "last_login",
    "commit", "merge_base", "patch_url", "diff_url", "merge_commit_sha", "merged_at", "closed_at",
    "due_date", "deadline", "target_commitish", "tarball_url", "zipball_url", "upload_url",
    "comments_url", "issue_url", "pull_request_url", "timeline", "content", "encoding",
    "download_url", "git_url", "_links", "last_commit_sha", "size", "sha1", "verification",
    "committer", "author", "repo_id", "issue_id", "poster_id", "user_id", "owner_id",
    "mergeable", "updated_at", "language", "languages_url", "fork_id", "commit_id",
}
# Lists Gitea builds from Go maps (their order changes from run to run).
UNORDERED = {"units", "events"}


def drop(k):
    return k in DROP or k.endswith("_url")


def unordered(v):
    if isinstance(v, dict):
        return {k: sorted(x) if k in UNORDERED and isinstance(x, list) else unordered(x) for k, x in v.items()}
    if isinstance(v, list):
        return [unordered(x) for x in v]
    return v


def step(name, method, path, body=None, keep=None):
    return g.step(name, method, path, body, drop=drop, keep=lambda b: unordered(keep(b) if keep else b))


def main():
    t = time.time()
    if not apps.wait_http(BASE + "/api/healthz", 900):
        apps.record("startup", "gitea never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("gitea up after %.1fs" % (time.time() - t), flush=True)
    r = subprocess.run([os.environ["GITEA_BIN"], "admin", "user", "create", "--config", os.environ["GITEA_CONF"],
                        "--username", "admin", "--password", PW, "--email", "admin@example.com", "--admin",
                        "--must-change-password=false"], capture_output=True, text=True)
    apps.record("cli:admin-create", {"code": r.returncode})

    app = psycopg.connect(host=PGHOST, port=PGPORT, user="postgres", password="pw", dbname="gitea_app")
    app.autocommit = True
    apps.db_rows(app, "db:version", "SELECT version FROM version")
    apps.schema_digest(app)

    step("user:self", "GET", "/user", keep=lambda b: {k: b.get(k) for k in ("login", "is_admin", "email")})
    for u in ("alice", "bob", "carol"):
        step("admin:user-create:" + u, "POST", "/admin/users", {
            "username": u, "email": u + "@example.com", "password": PW + u, "must_change_password": False,
            "full_name": u.title() + " Example"})
    step("admin:user-dup", "POST", "/admin/users", {"username": "alice", "email": "x@example.com", "password": PW})
    step("admin:users", "GET", "/admin/users?limit=50")
    step("users:search", "GET", "/users/search?q=a&limit=10")
    step("org:create", "POST", "/orgs", {"username": "acme", "full_name": "Acme", "visibility": "public"})
    step("org:dup", "POST", "/orgs", {"username": "acme"})
    st, team = step("org:team", "POST", "/orgs/acme/teams", {
        "name": "devs", "permission": "write", "units": ["repo.code", "repo.issues", "repo.pulls"]})
    tid = team.get("id") if isinstance(team, dict) else 0
    step("team:add-member", "PUT", "/teams/%s/members/alice" % tid)
    step("team:members", "GET", "/teams/%s/members" % tid)
    step("org:teams", "GET", "/orgs/acme/teams")

    for owner, name in (("user", "notes"), ("org", "widget"), ("org", "gadget")):
        path = "/user/repos" if owner == "user" else "/orgs/acme/repos"
        step("repo:create:" + name, "POST", path, {
            "name": name, "description": "the %s repository" % name, "auto_init": True,
            "readme": "Default", "default_branch": "main", "private": name == "gadget"})
    step("repo:dup", "POST", "/user/repos", {"name": "notes"})
    step("repo:missing", "GET", "/repos/acme/nothing")
    step("repo:get", "GET", "/repos/acme/widget")
    step("repo:search", "GET", "/repos/search?q=g&sort=alpha&order=asc")
    step("repo:search-private", "GET", "/repos/search?is_private=true")
    step("repo:file-create", "POST", "/repos/acme/widget/contents/src/main.txt", {
        "content": "aGVsbG8gd29ybGQK", "message": "add main", "branch": "main"})
    step("repo:branch", "POST", "/repos/acme/widget/branches", {"new_branch_name": "feature", "old_branch_name": "main"})
    step("repo:file-update", "POST", "/repos/acme/widget/contents/src/feature.txt", {
        "content": "ZmVhdHVyZQo=", "message": "add feature", "branch": "feature"})
    step("repo:branches", "GET", "/repos/acme/widget/branches")
    step("repo:tree", "GET", "/repos/acme/widget/contents/src?ref=feature")
    step("repo:commits", "GET", "/repos/acme/widget/commits?sha=feature&limit=10",
         keep=lambda b: len(b) if isinstance(b, list) else b)
    step("repo:collaborator", "PUT", "/repos/acme/widget/collaborators/bob", {"permission": "read"})
    step("repo:collaborators", "GET", "/repos/acme/widget/collaborators")

    labels = {}
    for name, color in (("bug", "#ee0701"), ("feature", "#84b6eb"), ("question", "#cc317c")):
        st, lb = step("label:" + name, "POST", "/repos/acme/widget/labels", {"name": name, "color": color})
        labels[name] = lb.get("id") if isinstance(lb, dict) else 0
    step("milestone", "POST", "/repos/acme/widget/milestones", {"title": "v1.0", "description": "first"})
    step("milestones", "GET", "/repos/acme/widget/milestones?state=all")
    for i in range(1, 9):
        body = {"title": "issue %d %s" % (i, "crash on save" if i % 3 == 0 else "slow search"),
                "body": "details of issue %d" % i, "labels": [labels["bug" if i % 2 else "feature"]],
                "assignees": ["alice"] if i % 4 == 0 else []}
        if i <= 4:
            body["milestone"] = 1
        step("issue:create:%d" % i, "POST", "/repos/acme/widget/issues", body)
    for i in (1, 2, 5):
        step("issue:comment:%d" % i, "POST", "/repos/acme/widget/issues/%d/comments" % i,
             {"body": "comment on %d" % i})
    step("issue:reaction", "POST", "/repos/acme/widget/issues/1/reactions", {"content": "+1"})
    step("issue:reactions", "GET", "/repos/acme/widget/issues/1/reactions")
    step("issue:close", "PATCH", "/repos/acme/widget/issues/2", {"state": "closed"})
    step("issue:edit", "PATCH", "/repos/acme/widget/issues/3", {"title": "issue 3 renamed", "body": "new body"})
    step("issue:label-add", "POST", "/repos/acme/widget/issues/3/labels", {"labels": [labels["question"]]})
    step("issues:open", "GET", "/repos/acme/widget/issues?state=open&type=issues&limit=50")
    step("issues:all", "GET", "/repos/acme/widget/issues?state=all&type=issues&limit=50")
    step("issues:by-label", "GET", "/repos/acme/widget/issues?state=all&labels=bug")
    step("issues:by-milestone", "GET", "/repos/acme/widget/issues?state=all&milestones=v1.0")
    step("issues:search-q", "GET", "/repos/acme/widget/issues?state=all&q=crash")
    step("issues:assigned", "GET", "/repos/issues/search?assigned=true&state=all", )
    step("issues:global", "GET", "/repos/issues/search?q=slow&state=all&limit=20")
    step("issue:comments", "GET", "/repos/acme/widget/issues/1/comments")
    step("issue:timeline", "GET", "/repos/acme/widget/issues/3/timeline",
         keep=lambda b: sorted(x.get("type", "") for x in b) if isinstance(b, list) else b)
    step("milestone:after", "GET", "/repos/acme/widget/milestones/1")
    step("labels", "GET", "/repos/acme/widget/labels")

    step("pull:create", "POST", "/repos/acme/widget/pulls", {
        "title": "add feature", "head": "feature", "base": "main", "body": "please merge"})
    step("pull:list", "GET", "/repos/acme/widget/pulls?state=all")
    step("pull:get", "GET", "/repos/acme/widget/pulls/9")
    step("pull:files", "GET", "/repos/acme/widget/pulls/9/files")
    step("pull:review", "POST", "/repos/acme/widget/pulls/9/reviews", {"event": "COMMENT", "body": "looks fine"})
    step("pull:merge", "POST", "/repos/acme/widget/pulls/9/merge", {"Do": "merge"})
    step("pull:after", "GET", "/repos/acme/widget/pulls/9", keep=lambda b: {
        k: b.get(k) for k in ("state", "merged", "title", "comments")} if isinstance(b, dict) else b)

    step("release:create", "POST", "/repos/acme/widget/releases", {
        "tag_name": "v1.0.0", "name": "1.0", "body": "first release", "target_commitish": "main"})
    step("releases", "GET", "/repos/acme/widget/releases")
    step("tags", "GET", "/repos/acme/widget/tags")
    step("star", "PUT", "/user/starred/acme/widget")
    step("starred", "GET", "/user/starred")
    step("stargazers", "GET", "/repos/acme/widget/stargazers")
    step("watch", "PUT", "/repos/acme/widget/subscription")
    step("hook:create", "POST", "/repos/acme/widget/hooks", {
        "type": "gitea", "active": False, "events": ["push", "issues"],
        "config": {"url": "http://%s:9/hook" % PGHOST, "content_type": "json"}})
    step("hooks", "GET", "/repos/acme/widget/hooks")
    step("org:repos", "GET", "/orgs/acme/repos")
    step("user:repos", "GET", "/user/repos")
    step("admin:orgs", "GET", "/admin/orgs")
    step("notifications", "GET", "/notifications?all=true",
         keep=lambda b: len(b) if isinstance(b, list) else b)
    step("feeds", "GET", "/users/admin/activities/feeds?limit=50",
         keep=lambda b: sorted(x.get("op_type", "") for x in b) if isinstance(b, list) else b)

    step("repo:delete", "DELETE", "/repos/acme/gadget")
    step("repo:after-delete", "GET", "/repos/acme/gadget")
    step("admin:user-delete", "DELETE", "/admin/users/carol")
    step("admin:users-after", "GET", "/admin/users?limit=50")
    apps.db_rows(app, "db:counts", """SELECT (SELECT count(*) FROM "user"), (SELECT count(*) FROM repository),
        (SELECT count(*) FROM issue), (SELECT count(*) FROM comment), (SELECT count(*) FROM label),
        (SELECT count(*) FROM action), (SELECT count(*) FROM release)""")
    apps.db_rows(app, "db:issue-stats", """SELECT num_issues, num_closed_issues, num_pulls, num_closed_pulls,
        num_milestones, num_stars, num_watches FROM repository ORDER BY lower_name""")


if __name__ == "__main__":
    main()
