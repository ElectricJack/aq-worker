"""Contract checks for the shipped C3c recipes, using the unchanged C3b API."""
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
RECIPES = ROOT / "packaging/sandbox/recipes"
spec = importlib.util.spec_from_file_location("template_tool", ROOT / "packaging/sandbox/template_tool.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


def build_config(parent):
    return {
        "schema_version": 1, "role": "template-build", "docker_context": "default",
        "buildkit_builder": "aq-c3c", "builder_identity": "c3c-local",
        "os_image": parent,
        "runtime_image": "ghcr.io/electricjack/aq-sandbox-runtime@sha256:" + "b" * 64,
        "target_repository": "ghcr.io/electricjack/aq-template",
        "registry_visibility": "public", "allowlist": "allowlist.candidate.json",
    }


@pytest.mark.parametrize("name", ["rust", "node"])
def test_seed_recipe_uses_c3b_contract_without_repository_objects(name):
    recipe = json.loads((RECIPES / (name + ".json")).read_text())
    assert recipe["name"] == name
    assert recipe["version"] == "1"
    assert recipe["prefetch"] == []
    assert recipe["parent"].startswith("ghcr.io/electricjack/aq-sandbox-os@sha256:")
    config = build_config(recipe["parent"])
    tool.validate_config(config)
    tool.validate_recipe(recipe, config)
    rendered = tool.dockerfile(recipe, recipe["parent"], config)
    assert rendered.index("COPY --from=") > rendered.rindex("RUN [", 0, rendered.index("COPY --from="))
    assert not tool.scan.secret_shaped(rendered)
