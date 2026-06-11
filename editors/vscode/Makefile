# Build, install, and uninstall the Coddl VSCode extension.
#
# Quick start:
#   make install      # compile + symlink into ~/.vscode/extensions/coddl
#   # restart VSCode (or "Developer: Reload Window")
#
# Dev loop while editing src/extension.ts:
#   make watch        # tsc keeps rebuilding; Reload Window picks up changes

EXT_DIR := $(abspath $(dir $(firstword $(MAKEFILE_LIST))))
LINK    := $(HOME)/.vscode/extensions/coddl

.DEFAULT_GOAL := help

.PHONY: help install uninstall compile watch package clean

help:
	@echo "Targets:"
	@echo "  install     compile and symlink the extension into ~/.vscode/extensions/coddl"
	@echo "  uninstall   remove the symlink"
	@echo "  compile     run tsc once"
	@echo "  watch       run tsc in watch mode (Reload Window in VSCode to pick up changes)"
	@echo "  package     produce coddl.vsix via @vscode/vsce"
	@echo "  clean       remove node_modules, out, *.vsix"
	@echo ""
	@echo "Symlink target: $(LINK)"
	@echo "Extension dir : $(EXT_DIR)"

install: compile
	@mkdir -p "$(dir $(LINK))"
	@if [ -L "$(LINK)" ]; then \
		rm -f "$(LINK)"; \
	elif [ -e "$(LINK)" ]; then \
		echo "error: $(LINK) exists and is not a symlink; refusing to overwrite" >&2; \
		exit 1; \
	fi
	@ln -s "$(EXT_DIR)" "$(LINK)"
	@echo "linked: $(LINK) -> $(EXT_DIR)"
	@echo "restart VSCode (or 'Developer: Reload Window') to pick up the extension"

uninstall:
	@if [ -L "$(LINK)" ]; then \
		rm -f "$(LINK)"; \
		echo "removed: $(LINK)"; \
	elif [ -e "$(LINK)" ]; then \
		echo "error: $(LINK) exists but isn't a symlink; refusing to remove" >&2; \
		exit 1; \
	else \
		echo "$(LINK) doesn't exist; nothing to do"; \
	fi

compile: node_modules
	npm run compile

watch: node_modules
	npm run watch

node_modules: package.json
	npm install --no-audit --no-fund
	@touch node_modules

package: compile
	npx --yes @vscode/vsce package --skip-license -o coddl.vsix

clean:
	rm -rf node_modules out coddl.vsix
