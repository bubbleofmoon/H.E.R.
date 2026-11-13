@echo off
chcp 65001 >nul
title QQ Bot Stopper
powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0stop-bot.ps1" %*
exit /b %ERRORLEVEL%
