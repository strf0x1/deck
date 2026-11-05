#!/usr/bin/env python3
"""Build an acceptance guest from Deck's recipe and a verified Linux Construct binary."""
import argparse
from pathlib import Path
import shlex
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--construct-binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.construct_binary.resolve(strict=True)
    root = Path(__file__).resolve().parents[2]
    recipe = (root / 'scripts/vm-image.sh').read_text()
    start = recipe.index('echo "building construct')
    end = recipe.index('# deck-egress', start)
    recipe = recipe[:start] + 'cp ' + shlex.quote(str(binary)) + ' "$scratch/construct"\n\n' + recipe[end:]
    recipe = '\n'.join(line for line in recipe.split('\n')
                       if not line.startswith('[ -d "$construct_src/crates/construct" ]')
                       and not line.startswith('construct_src=$(cd '))
    recipe = recipe.replace('deck_src=$(cd "$(dirname "$0")/.." && pwd)',
                            'deck_src=' + shlex.quote(str(root)))
    recipe = recipe.replace('findutils tar gzip xz-utils less file',
                            'findutils tar gzip xz-utils less file curl python3 openssh-client iproute2 iptables')
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / 'acceptance-image-recipe.sh').write_text(recipe)
    with tempfile.NamedTemporaryFile(mode='w', suffix='.sh') as script:
        script.write(recipe)
        script.flush()
        subprocess.run(['sh', script.name, '.', str(args.output.resolve())], check=True)


if __name__ == '__main__':
    main()
