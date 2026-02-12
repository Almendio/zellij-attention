# Troubleshooting

## Notifications not appearing

1. **Check plugin is loaded:**
   - Verify the plugin pane exists in your layout's `default_tab_template`
   - Check Zellij logs: `tail -f /tmp/zellij-*/zellij-log-*/zellij.log` (look for `zellij-attention: loaded`)

2. **Verify pipe commands work:**
   ```bash
   echo $ZELLIJ_PANE_ID  # Should print a number
   zellij pipe --name "zellij-attention::waiting::$ZELLIJ_PANE_ID"
   # Tab name should change immediately
   ```

3. **Check state file:**
   ```bash
   # State is persisted in the directory where Zellij was launched
   # /host/ in the plugin maps to your cwd
   ls -la .zellij-attention-state.bin
   ```

4. **Clear Zellij plugin cache:**
   ```bash
   # Zellij caches compiled WASM — clear if the plugin isn't updating
   find ~/.cache/zellij -path "*zellij-attention*" -exec rm -f {} \;
   ```

## Plugin not loading

- The plugin **must** be in a layout pane (in `default_tab_template`), NOT in the `load_plugins` section of `config.kdl`
- `load_plugins` does NOT pass configuration to plugins — use layout panes instead

## Pipe command hangs or does nothing

- Ensure you're using the `--name` flag (broadcast), NOT `--plugin` (targeted)
- Check `$ZELLIJ_PANE_ID` is set: `echo $ZELLIJ_PANE_ID`
- Verify the format uses double-colon separators: `zellij-attention::EVENT_TYPE::PANE_ID`

### Wrong format examples

**Correct:**
```bash
zellij pipe --name "zellij-attention::waiting::5"
```

**Common mistakes:**
```bash
# WRONG: Single colon
zellij pipe --name "zellij-attention:waiting:5"

# WRONG: Missing plugin name prefix
zellij pipe --name "waiting::5"

# WRONG: Using --plugin instead of --name
zellij pipe --plugin "zellij-attention" --message "waiting::5"
```

## Tabs not restoring original names

- This is expected if notifications are still active on other panes in the same tab
- Focus the pane with the notification to clear it — the tab name restores automatically
- To force-clear all notifications: `rm .zellij-attention-state.bin` in the directory where Zellij was launched, then restart

## Multiple plugin instances

This is normal — one instance per tab via `default_tab_template`. All instances share state through `/host/.zellij-attention-state.bin`. Broadcast pipes (`--name`) reach all instances simultaneously.
