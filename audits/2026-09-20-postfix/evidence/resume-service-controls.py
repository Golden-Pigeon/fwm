"""Run selected existing service transaction controls with the fake manager.

Only rustc and the resulting local test executable are spawned. Service commands
are captured by the repository's FakeExecutor; no native manager is invoked.
"""
from pathlib import Path
import json
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / "crates/fwm/src/platform"
CONTROLS = [
    "outer_checkpoint_recovers_old_definition_and_manager_after_failed_upgrade",
    "rollback_ownership_check_precedes_manager_changes",
    "invalid_definition_destination_fails_before_any_manager_mutation",
    "restoring_an_inactive_mac_registration_does_not_wake_the_daemon",
    "rollback_restores_disabled_login_policy_even_when_the_old_job_was_unloaded",
    "query_errors_are_not_reported_as_absent_registrations",
    "unrecoverable_orphan_is_rejected_before_stopping_the_old_service",
    "unavailable_manager_allows_unmanaged_start_only_without_saved_definition",
]


def source_paths(text):
    return re.sub(r'#\[path = "([^"]+)"\]',
                  lambda m: '#[path = "' + str(SOURCE / m[1]) + '"]', text)


def main():
    with tempfile.TemporaryDirectory(prefix="fwm-resume-service-", dir="/private/tmp") as tmp:
        directory = Path(tmp)
        fixtures = directory / "service_tests.rs"
        fixtures.write_text(source_paths((SOURCE / "service_tests.rs").read_text()))
        service = source_paths((SOURCE / "service.rs").read_text())
        service = service.replace(str(SOURCE / "service_tests.rs"), str(fixtures))
        (directory / "service.rs").write_text(service)
        (directory / "main.rs").write_text('#[path = "service.rs"]\nmod service;\n')
        dependencies = ROOT / "target/debug/deps"
        command = ["rustc", "--edition=2024", "--test", str(directory / "main.rs"),
                   "-L", "dependency=" + str(dependencies), "-o", str(directory / "controls"),
                   "-A", "dead_code"]
        for name in ["anyhow", "fwm_core", "serde", "serde_json", "uuid", "tempfile", "directories", "libc", "fs2"]:
            library = max(dependencies.glob("lib" + name + "-*.rlib"), key=lambda p: p.stat().st_mtime)
            command.extend(["--extern", name + "=" + str(library)])
        subprocess.run(command, check=True, cwd=ROOT, capture_output=True, text=True, timeout=60)
        results = []
        for test in CONTROLS:
            exact = "service::workflow_tests::" + test
            result = subprocess.run([str(directory / "controls"), exact, "--exact", "--nocapture"],
                                    cwd=ROOT, capture_output=True, text=True, timeout=15)
            assert result.returncode == 0 and "1 passed" in result.stdout, result.stdout + result.stderr
            results.append({"test": exact, "returncode": result.returncode,
                            "stdout": result.stdout, "stderr": result.stderr})
    output = {"scope": "existing pure fake-manager service transaction controls",
              "native_service_manager_invoked": False, "tests": results,
              "passed": len(results), "temporary_directory_cleaned": True}
    Path(__file__).with_suffix(".json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed": len(results), "native_service_manager_invoked": False}))


if __name__ == "__main__":
    main()
