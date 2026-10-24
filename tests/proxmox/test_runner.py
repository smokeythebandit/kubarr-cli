"""Target guards must fail before any Proxmox mutation or local preparation."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
import shutil
import subprocess
import tempfile
import yaml
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('kubarr_proxmox_lab', Path(__file__).with_name('run.py'))
lab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lab)


class TargetGuardTests(unittest.TestCase):
    def test_wrong_name_or_uuid_never_stops_a_vm(self):
        for config in [
            {'name': 'another-vm', 'smbios1': 'uuid=' + lab.UUID},
            {'name': 'kubarr-prod', 'smbios1': 'uuid=another-uuid'},
        ]:
            with self.subTest(config=config), patch.object(lab, 'api', return_value=config), patch.object(lab, 'task') as task:
                with self.assertRaisesRegex(RuntimeError, 'identity'):
                    lab.run(SimpleNamespace(), Path('/unused'))
                task.assert_not_called()

    def test_missing_snapshot_never_stops_a_vm(self):
        responses = [{'name': 'kubarr-prod', 'smbios1': 'uuid=' + lab.UUID}, []]
        with patch.object(lab, 'api', side_effect=responses), patch.object(lab, 'task') as task:
            with self.assertRaisesRegex(RuntimeError, 'snapshot is missing'):
                lab.run(SimpleNamespace(), Path('/unused'))
            task.assert_not_called()

    def test_candidate_catalog_uses_real_directories_and_renderable_init_container(self):
        source = lab.ROOT.parent / 'kubarr-charts' / 'system'
        with tempfile.TemporaryDirectory(prefix='kubarr-catalog-test-', dir='/tmp/opencode') as directory:
            root = Path(directory)
            for component in ('kubarr-backend', 'kubarr-worker'):
                shutil.copytree(source / component, root / component)
            lab.configure_catalog_mount(root, 'kubarr-lab-charts', {'sonarr.yaml': 'name: sonarr\nversion: 1.0.0\n'}, {'sonarr': '1.0.0'})
            for component in ('kubarr-backend', 'kubarr-worker'):
                output = subprocess.check_output(['helm', 'template', component, str(root / component)], text=True)
                resources = [item for item in yaml.safe_load_all(output) if item]
                pod = next(item['spec']['template']['spec'] for item in resources if item['kind'] == 'Deployment')
                init, = pod['initContainers']
                self.assertEqual(init['command'], ['sh', '-ec', 'cp -RL /metadata/* /catalog/'])
                self.assertEqual(init['securityContext']['runAsUser'], 1000)
                volume = next(volume for volume in pod['volumes'] if volume['name'] == 'lab-charts')
                self.assertEqual(volume['emptyDir'], {})
                container, = pod['containers']
                mount = next(mount for mount in container['volumeMounts'] if mount['name'] == 'lab-charts')
                self.assertTrue(mount['readOnly'])
                env = {item['name']: item.get('value') for item in container['env']}
                self.assertEqual(env['KUBARR_CHARTS_SOURCE_DIR'], '/lab-charts')
