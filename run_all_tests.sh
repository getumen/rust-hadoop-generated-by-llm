#!/bin/bash

# Script to run all test scripts in test_scripts/ directory sequentially
# Stops on first failure due to set -e

set -e

echo "🧪 Running all test scripts in test_scripts/ directory..."
echo "======================================================="

# Get the list of scripts in alphabetical order
scripts=$(ls test_scripts/*.sh | sort)

for script in $scripts; do
    echo ""
    echo "▶️  Running $script..."
    echo "----------------------------------------"
    bash "$script"
    echo "✅ $script completed successfully"
done

echo ""
echo "🎉 All test scripts completed successfully!"
