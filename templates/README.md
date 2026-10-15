# rsy Ignore Templates

This directory contains example ignore templates for common project types.

## Usage

### Option 1: Copy to your config directory

```bash
# Create templates directory
mkdir -p ~/.config/rsy/templates

# Copy templates you want
cp templates/*.rsyignore ~/.config/rsy/templates/

# Use with --ignore-template flag
rsy /source /dest --ignore-template rust
rsy /source /dest --ignore-template node --ignore-template python
```

### Option 2: Use .rsyignore files in your project

Create a `.rsyignore` file in your project root (similar to `.gitignore`):

```bash
# Create .rsyignore
cat > /path/to/project/.rsyignore <<'EOF'
# Project-specific ignore patterns
target/
node_modules/
__pycache__/
EOF

# rsy will automatically load .rsyignore from the source directory
rsy /path/to/project /backup
```

## Available Templates

- **rust.rsyignore** - For Rust projects (target/, Cargo.lock, etc.)
- **node.rsyignore** - For Node.js projects (node_modules/, dist/, logs/, etc.)
- **python.rsyignore** - For Python projects (\_\_pycache\_\_/, .venv/, dist/, etc.)

## Pattern Syntax

Templates use rsync-style filter syntax:

```
# Comments start with #
*.log          # Exclude all .log files
target/        # Exclude target directory and all contents
+ *.txt        # Include .txt files (use + for explicit includes)
- *.tmp        # Exclude .tmp files (- prefix optional, exclude is default)
```

## Priority Order

When multiple filter sources are used, they are applied in this order:

1. `--filter` CLI flags (highest priority)
2. `--include` and `--exclude` CLI flags
3. `.rsyignore` file in source directory
4. `--ignore-template` templates
5. `.gitignore` patterns (lowest priority)

First matching rule wins!

## Creating Custom Templates

Create your own templates in `~/.config/rsy/templates/`:

```bash
# Create a custom template
cat > ~/.config/rsy/templates/myproject.rsyignore <<'EOF'
# Custom ignore patterns for my project
build/
cache/
*.tmp
EOF

# Use it
rsy /source /dest --ignore-template myproject
```

## Examples

```bash
# Rust project backup
rsy ~/rust-project ~/backups/rust --ignore-template rust

# Node.js project with custom .rsyignore
cd ~/node-app
echo "node_modules/" > .rsyignore
echo "dist/" >> .rsyignore
rsy . ~/backups/node-app

# Multiple templates
rsy ~/monorepo ~/backups --ignore-template rust --ignore-template node

# Combine with CLI patterns
rsy ~/project ~/backup --ignore-template rust --exclude "*.bak"
```
