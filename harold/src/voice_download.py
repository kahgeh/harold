# Harold's speech-model download. Harold writes this file into its speech-models folder and
# runs it as `<python> -I voice_download.py <org>/<name> <commit> <record> [--check]`, with
# an environment of PATH, HOME, HF_HOME and HF_HUB_CACHE only. It prints JSON lines on stdout:
#   {"total": <bytes>}                     the size of the commit's files
#   {"done": true, "bytes": <n>}           every file is in the cache and checked; the record is written
#   {"done": false, "reason": "<word>"}    network, not_found, disk_full, check_failed, not_allowed,
#                                          failed, or (with --check only) missing
# With --check it downloads nothing: it only checks the cached files.
# With --list it downloads and checks nothing: it prints {"files": [{"path": <name>, "size":
# <bytes>}, ...]} for the commit and then {"done": true, "bytes": <n>}; the record is not written.
# It sends no token and no telemetry, asks only for the commit it was given, and checks
# every file the commit lists, by size and, for large files, by sha256.
import errno
import hashlib
import json
import os
import re
import sys

# Before huggingface_hub is imported, so the library neither reads a token from the Mac
# nor reports telemetry.
os.environ["HF_HUB_DISABLE_IMPLICIT_TOKEN"] = "1"
os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
os.environ["HF_HUB_DISABLE_PROGRESS_BARS"] = "1"

from huggingface_hub import HfApi, snapshot_download  # noqa: E402

MODEL = re.compile(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
CHUNK = 1024 * 1024


def say(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


def fail(reason):
    say({"done": False, "reason": reason})
    sys.exit(0)


def reason_of(error):
    """The word for why a call to the Hub or the disk failed."""
    names = {cls.__name__ for cls in type(error).__mro__}
    if isinstance(error, OSError) and getattr(error, "errno", None) == errno.ENOSPC:
        return "disk_full"
    if names & {"RepositoryNotFoundError", "RevisionNotFoundError", "EntryNotFoundError"}:
        return "not_found"
    if "GatedRepoError" in names:
        return "not_allowed"
    status = getattr(getattr(error, "response", None), "status_code", None)
    if status in (401, 403):
        return "not_allowed"
    if status == 404:
        return "not_found"
    if any(
        word in name
        for name in names
        for word in ("Connection", "Timeout", "SSL", "Proxy", "Offline", "NetworkError")
    ):
        return "network"
    if status is not None and status >= 500:
        return "network"
    return "failed"


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        while True:
            block = file.read(CHUNK)
            if not block:
                break
            digest.update(block)
    return digest.hexdigest()


def listed_sha256(sibling):
    lfs = getattr(sibling, "lfs", None)
    if not lfs:
        return None
    value = getattr(lfs, "sha256", None)
    if value is None:
        try:
            value = lfs["sha256"]
        except (KeyError, TypeError):
            value = None
    return value


def check_files(info, folder):
    """Every file the commit lists is in `folder` with its size, and each large file's bytes
    match the sha256 listed for it. Returns the record's files, or None."""
    files = []
    for sibling in info.siblings:
        path = os.path.join(folder, sibling.rfilename)
        if not os.path.isfile(path) or os.path.getsize(path) != sibling.size:
            return None
        digest = sha256_of(path)
        wanted = listed_sha256(sibling)
        if wanted is not None and digest != wanted:
            return None
        files.append({"path": sibling.rfilename, "size": sibling.size, "sha256": digest})
    return files


def write_record(record, model, commit, files):
    body = json.dumps({"model": model, "commit": commit, "files": files})
    temporary = record + ".tmp"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w") as file:
        file.write(body)
    os.replace(temporary, record)


def main():
    arguments = [argument for argument in sys.argv[1:] if argument not in ("--check", "--list")]
    check_only = "--check" in sys.argv[1:]
    list_only = "--list" in sys.argv[1:]
    if len(arguments) != 3:
        fail("failed")
    model, commit, record = arguments
    if not MODEL.match(model) or not COMMIT.match(commit):
        fail("not_allowed")
    try:
        info = HfApi(token=False).model_info(
            model, revision=commit, files_metadata=True, token=False
        )
    except Exception as error:
        fail(reason_of(error))
    if info.sha != commit:
        fail("check_failed")
    total = sum(sibling.size or 0 for sibling in info.siblings)
    if list_only:
        say(
            {
                "files": [
                    {"path": sibling.rfilename, "size": sibling.size or 0}
                    for sibling in info.siblings
                ]
            }
        )
        say({"done": True, "bytes": total})
        return
    say({"total": total})
    cache = os.environ.get("HF_HUB_CACHE")
    if not cache:
        from huggingface_hub import constants

        cache = constants.HF_HUB_CACHE
    folder = os.path.join(cache, "models--" + model.replace("/", "--"), "snapshots", commit)
    if check_only:
        files = check_files(info, folder)
        if files is None:
            fail("missing")
        write_record(record, model, commit, files)
        say({"done": True, "bytes": total})
        return
    try:
        path = snapshot_download(model, revision=commit, token=False)
    except Exception as error:
        fail(reason_of(error))
    if not str(path).rstrip("/").endswith("snapshots/" + commit):
        fail("check_failed")
    files = check_files(info, str(path))
    if files is None:
        fail("check_failed")
    write_record(record, model, commit, files)
    say({"done": True, "bytes": total})


main()
