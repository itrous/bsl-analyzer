#!/usr/bin/python3 -I
"""Deterministic process double; never reads a real 1C infobase."""
import json
from pathlib import Path
import sys

args = sys.argv[1:]
cwd = Path.cwd()


def value(option):
    return args[args.index(option) + 1]


with (cwd / "platform-calls.jsonl").open("a", encoding="utf-8") as calls:
    calls.write(json.dumps(args) + "\n")

if Path(sys.argv[0]).name == "ibcmd":
    if args == ["--version"]:
        print("8.3.27.1989")
    elif args[:2] == ["extension", "list"]:
        print('''name : "FixtureExtension"
version :
active : yes
purpose : customization
safe-mode : yes
security-profile-name :
unsafe-action-protection : yes
used-in-distributed-infobase : no
scope : infobase
hash-sum : "fixture-hash"
''')
    elif args[:2] != ["extension", "update"]:
        sys.exit(2)
    sys.exit(0)

assert args[0] in ("DESIGNER", "CREATEINFOBASE")
log = Path(value("/Out"))
if args[0] == "CREATEINFOBASE":
    ib = Path(args[1].removeprefix('File="').removesuffix('";'))
    ib.mkdir()
    (ib / "1Cv8.1CD").write_bytes(b"fake attempt database")
else:
    ib = Path(value("/F"))
    # A read-like Designer command can write both the database and sidecars.
    with (ib / "1Cv8.1CD").open("ab") as db:
        db.write(b"platform service write")
    (ib / "platform-service-file").write_bytes(b"service")
    if "/DumpDBCfgList" in args:
        log.write_text("FixtureExtension\n", encoding="utf-8")
    elif "/DumpDBCfg" in args:
        output = Path(value("/DumpDBCfg"))
        if output.name == "source-verify.cf" and (cwd.parent.parent / "inject-source-drift").exists():
            import time
            (cwd / "recheck-ready").touch()
            deadline = time.monotonic() + 10
            while not (cwd / "recheck-continue").exists():
                if time.monotonic() >= deadline:
                    sys.exit(2)
                time.sleep(0.01)
        output.write_bytes(b"stable fake configuration")
    elif "/DumpConfigToFiles" in args:
        xml = Path(value("/DumpConfigToFiles"))
        (xml / "Configuration.xml").write_text('''<Configuration><Properties>
<ScriptVariant>English</ScriptVariant><CompatibilityMode>Version8_3_27</CompatibilityMode>
</Properties></Configuration>''', encoding="utf-8")
        (xml / "CommonForms").mkdir()
        (xml / "CommonForms" / "Fixture.xml").write_text('''<MetaDataObject><CommonForm>
<Properties><FormType>Ordinary</FormType></Properties></CommonForm></MetaDataObject>''', encoding="utf-8")
    elif "/CheckModules" in args:
        if log.name.startswith("calibration-"):
            log.write_text("{CommonForm.Fixture.Form(2,1)}: Synthetic calibration error\n", encoding="utf-8")
            sys.exit(101)
        log.write_text("No syntax errors found!\n", encoding="utf-8")
