# Programmatic Tool Calling (Unified Exec)

This document describes the implementation of Programmatic Tool Calling, a feature that allows the model to write scripts (e.g., Python) that natively invoke other registered tools via an IPC mechanism.

## 1. How the Original Unified Exec Worked

The original `unified_exec` implementation was primarily a session manager for persistent PTYs (Pseudo-Terminals).

*   **Mechanism**: It spawned processes (like `bash` or `python`) wrapped in a PTY and kept them alive between turns.
*   **State**: It allowed the model to maintain state (e.g., setting environment variables, defining functions) across multiple `exec_command` calls.
*   **Limitation**: The interaction was strictly **unidirectional** from the Agent to the Script. The script could print output, but it could not "call back" to the Agent to request services (like reading a file or searching the web). If the model wanted to use a tool based on script output, it had to:
    1.  Run the script.
    2.  Wait for the turn to finish.
    3.  Read the output in the next turn.
    4.  Call the tool.
    5.  Feed the result back into a new script command.

## 2. What We Did (Implementation Changes)

We extended the `unified_exec` runtime to support bidirectional communication, effectively turning the Agent into an operating system for the running script.

### Key Changes

1.  **Tool Dispatch Capability**:
    *   Defined a `ToolDispatcher` trait in `core/src/tools/context.rs` and added a `dispatcher` field to `ToolInvocation`.
    *   Implemented `ToolDispatcher` for `ToolRouter` in `core/src/tools/router.rs` to allow recursive tool dispatching.

2.  **Interactive Execution Manager**:
    *   Modified `UnifiedExecSessionManager` in `core/src/unified_exec/session_manager.rs`.
    *   Replaced the simple `collect_output_until_deadline` loop with a smarter `interact_until_deadline`.
    *   This new method continuously monitors the process output for a specific IPC pattern (`<<TOOL_CALL>>`).
    *   When a tool call is detected, it **pauses** execution, invokes the requested tool using the `dispatcher`, and writes the result back to the process's standard input using the pattern (`<<TOOL_RESULT>>`).

3.  **Python Helper Generation**:
    *   Updated `UnifiedExecHandler` in `core/src/tools/handlers/unified_exec.rs`.
    *   Before executing a command, it now generates a `codex_tools.py` file in a temporary directory and sets `PYTHONPATH`.
    *   This file contains Python function stubs for **all** registered tools (including dynamic MCP tools).
    *   These stubs handle the serialization of arguments and parsing of results via the IPC protocol.

4.  **Integration**:
    *   Updated `exec_command` and `write_stdin` to propagate the dispatcher and tracker down to the session manager.
    *   Updated other tool handlers (`apply_patch`, `shell`) to accommodate the changes in the `ToolInvocation` struct.

## 3. The Benefits

The dynamic script generation and IPC layer provide critical advantages over the raw PTY session:

### 1. Hides Complexity (The IPC Protocol)
*   **Without the update**: The model would have to manually print raw IPC strings like `print("<<TOOL_CALL>>" + json.dumps(...) + "<<END_TOOL_CALL>>")` and then write complex code to read `sys.stdin` to wait for the result. This is error-prone and wastes tokens.
*   **With the update**: The generated `codex_tools.py` abstracts all that away. The model just calls `codex_tools.shell(...)` like a normal function.

### 2. Dynamic Tool Availability
*   The `UnifiedExecHandler` generates the Python stubs based on the **currently registered tools** (which might include dynamic MCP tools loaded at runtime).
*   This means if you add a new tool to Codex (or connect a new MCP server), it **automatically** becomes available to the Python script without you having to teach the model a new protocol.

### 3. Efficiency
*   By writing the helper library to a temporary file and setting `PYTHONPATH`, we inject this capability with **zero token cost** and **zero latency** at the start of the session.
*   The alternative (the model typing out the helper functions into the terminal every time) would be slow and expensive.

### Summary
*   **Original Code**: Worked fine for running simple commands, but scripts were isolated—they couldn't "talk back" to the agent.
*   **Updated Code**: Enables the agent to act as an operating system for the script, allowing for powerful, self-contained automation loops.

## 4. How to Use This

### Configuration
Ensure the feature is enabled in your `config.toml`:
```toml
[features]
unified_exec = true
```

### Model Usage
The model can now generate and execute Python scripts that import `codex_tools`.

**Example: Multi-step file processing in a single turn**

```python
import codex_tools
import json

# 1. Use the shell tool to find files
find_result = codex_tools.shell(command=["find", "src", "-name", "*.rs"])
files = find_result['output'].strip().split('\n')

print(f"Found {len(files)} files. Analyzing...")

# 2. Iterate and process locally
for file_path in files[:5]:
    # 3. Call 'read_file' tool for each file (natively, without new turns)
    # Note: Assuming 'read_file' is an available tool
    content = codex_tools.read_file(file_path=file_path)
    
    if "TODO" in content:
        print(f"Found TODO in {file_path}")
```

When the agent runs this script via `exec_command`, it will execute the `find` command, return the list, and then let the script loop through the files, calling `read_file` for each one, all within a single `exec_command` interaction.
