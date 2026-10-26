import argparse
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from run import Journey, ROOT, admission_config, chart_versions, container_images


class HarnessTests(unittest.TestCase):
    def test_all_installed_releases_have_unambiguous_pins(self):
        pins = chart_versions((ROOT / "src/charts.rs").read_text())
        self.assertIn("+", pins["fluent-bit"])
        with self.assertRaises(ValueError):
            chart_versions('"fluent-bit" => "5.1.1+9"')

    def test_preload_covers_nested_workloads_and_init_containers(self):
        manifest = [{"kind": "Deployment", "spec": {"template": {"spec": {
            "initContainers": [{"name": "setup", "image": "busybox:1.37.0"}],
            "containers": [{"name": "app", "image": "example/app:42"}],
        }}}}, {"kind": "ConfigMap", "data": {"image": "not-a-container"}}]
        self.assertEqual(container_images(manifest), {"busybox:1.37.0", "example/app:42"})

    def test_admission_reproduces_baseline_without_exempting_fluent_bit(self):
        policy = admission_config()["plugins"][0]["configuration"]
        self.assertEqual(policy["defaults"]["enforce"], "baseline")
        self.assertNotIn("fluent-bit", policy["exemptions"]["namespaces"])

    def journey(self, directory):
        return Journey(argparse.Namespace(artifacts=Path(directory), charts=None, binary=Path("/unused")))

    def test_isolates_context_and_redacts_credentials(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, KUBECONFIG="/production/config"):
            journey = self.journey(directory)
            try:
                self.assertNotEqual(journey.env["KUBECONFIG"], os.environ["KUBECONFIG"])
                self.assertTrue(Path(journey.env["KUBECONFIG"]).is_relative_to(journey.work))
                journey.invitation = "http://127.0.0.1:1234/#secret-invitation"
                output = journey.redact(f"{journey.password} {journey.invitation}")
                self.assertNotIn(journey.password, output)
                self.assertNotIn("secret-invitation", output)
            finally:
                journey.cleanup()

    def test_cleanup_removes_clients_before_server_and_only_owned_resources(self):
        with tempfile.TemporaryDirectory() as directory:
            journey = self.journey(directory)
            journey.cluster_created = journey.nfs_created = journey.volume_created = True
            with patch.object(journey, "run") as run:
                journey.cleanup()
            calls = [call.args for call in run.call_args_list]
            self.assertEqual(calls, [
                ("kind", "delete", "cluster", "--name", journey.name),
                ("docker", "rm", "-f", journey.name + "-nfs"),
                ("docker", "volume", "rm", journey.name),
            ])

    def test_failed_cluster_cleanup_keeps_nfs_available_to_remaining_clients(self):
        with tempfile.TemporaryDirectory() as directory:
            journey = self.journey(directory)
            journey.cluster_created = journey.nfs_created = True
            with patch.object(journey, "run", side_effect=RuntimeError("cluster still exists")) as run:
                with self.assertRaises(RuntimeError):
                    journey.cleanup()
                self.assertEqual(run.call_count, 1)
            journey.cluster_created = journey.nfs_created = False
            journey.cleanup()

    def test_nfs_fixture_waits_for_recovery_grace_without_changing_server_state(self):
        with tempfile.TemporaryDirectory() as directory:
            journey = self.journey(directory)
            try:
                with patch.object(journey, "run", side_effect=["N\n", "Y\n"]) as run, patch("run.time.sleep") as sleep:
                    journey.wait_nfs_grace()
                self.assertEqual(run.call_count, 2)
                self.assertEqual(run.call_args.args, (
                    "docker", "exec", journey.name + "-nfs", "sh", "-c",
                    "cat /proc/fs/nfsd/v4_end_grace",
                ))
                sleep.assert_called_once_with(2)
            finally:
                journey.cleanup()

    def test_nfs_fixture_grace_wait_has_a_deadline(self):
        with tempfile.TemporaryDirectory() as directory:
            journey = self.journey(directory)
            try:
                with patch.object(journey, "run", return_value="N"), patch("run.time.sleep"), patch("run.time.monotonic", side_effect=[0, 1, 121]):
                    with self.assertRaisesRegex(RuntimeError, "recovery grace"):
                        journey.wait_nfs_grace()
            finally:
                journey.cleanup()

    def test_nfs_fixture_uses_media_user_permissions_and_root_squashing(self):
        with tempfile.TemporaryDirectory() as directory:
            journey = self.journey(directory)
            try:
                with patch.object(journey, "run", return_value="/nfsshare *(rw,root_squash)") as run:
                    journey.configure_nfs_permissions()
                command = run.call_args.args[-1]
                self.assertIn("chown 1000:1000 /nfsshare", command)
                self.assertIn("chmod 0775 /nfsshare", command)
                self.assertIn("exportfs -ra; exportfs -v", command)
                for invalid in ("/nfsshare *(rw,no_root_squash)", ""):
                    with patch.object(journey, "run", return_value=invalid):
                        with self.assertRaisesRegex(RuntimeError, "root squashing"):
                            journey.configure_nfs_permissions()
            finally:
                journey.cleanup()


if __name__ == "__main__":
    unittest.main()
