#!/bin/bash

# Script to run all test scripts in test_scripts/ directory sequentially
# Stops on first failure due to set -e

set -e

# -------------------------------------------------------
# Global Docker cleanup: run before each test to ensure
# no stale containers or ports from a previous test cause
# the next one to fail.
# -------------------------------------------------------
docker_cleanup() {
    echo "🧹 [run_all_tests] Cleaning up Docker state before next test..."
    # Bring down all known compose stacks
    docker compose                               down -v 2>/dev/null || true
    docker compose -f docker-compose.toxiproxy-sharded.yml down -v 2>/dev/null || true
    if [ -f docker-compose.auto-scaling.yml ]; then
        docker compose -f docker-compose.auto-scaling.yml down -v 2>/dev/null || true
    fi
    # Remove any remaining project containers by label
    docker ps -aq --filter "name=dfs-" | xargs docker rm -f 2>/dev/null || true
    # Prune orphaned networks
    docker network prune -f 2>/dev/null || true
    echo "🧹 [run_all_tests] Docker cleanup done."
}

echo "🧪 Running all test scripts in test_scripts/ directory..."
echo "======================================================="

# Initial cleanup before the first test
docker_cleanup

# Get the list of scripts in alphabetical order
scripts=$(ls test_scripts/*.sh | sort)

for script in $scripts; do
    echo ""
    echo "▶️  Running $script..."
    echo "----------------------------------------"
    bash "$script"
    echo "✅ $script completed successfully"
    # Cleanup Docker state between tests
    docker_cleanup
done

echo ""
echo "🎉 All test scripts completed successfully!"
