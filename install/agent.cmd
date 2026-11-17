@echo off
rem winstats agent installer for the Windows command prompt: runs install\agent.ps1.
rem   curl -fsSL https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.cmd -o agent.cmd && agent.cmd
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/mgvs/winstats/main/install/agent.ps1 | iex"
