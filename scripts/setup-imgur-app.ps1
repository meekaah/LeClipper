<#
.SYNOPSIS
  One-time setup of the Imgur API app that gets compiled into screen2imgur.

.DESCRIPTION
  Imgur has no API for creating apps: registration is a web form that needs your
  logged-in Imgur account. This script opens that form and shows what to enter.
  It then validates the Client ID you paste back against the Imgur API and saves
  both values to imgur-app.env (gitignored). build.rs reads that file, so a plain
  `cargo build --release` bakes the credentials in.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\setup-imgur-app.ps1
#>
[CmdletBinding()]
param(
    [string]$AppName = "screen2imgur"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$envFile = Join-Path $root "imgur-app.env"
$callback = "http://localhost:47892/callback"
$formUrl = "https://api.imgur.com/oauth2/addclient"

function Test-ClientId([string]$id) {
    try {
        $r = Invoke-RestMethod -Uri "https://api.imgur.com/3/credits" -Headers @{ Authorization = "Client-ID $id" }
        return $r.data
    } catch {
        $code = $_.Exception.Response.StatusCode.value__
        if ($code -eq 403 -or $code -eq 401) { return $null }
        throw "Couldn't reach Imgur to validate the Client ID: $($_.Exception.Message)"
    }
}

if (Test-Path $envFile) {
    $answer = Read-Host "imgur-app.env already exists. Replace it? [y/N]"
    if ($answer -notmatch '^[yY]') { Write-Host "Kept the existing file."; exit 0 }
}

Write-Host ""
Write-Host "Step 1: register the app" -ForegroundColor Cyan
Write-Host "Your browser will open $formUrl (log in to Imgur if asked)."
Write-Host "Fill in the form like this:"
Write-Host ""
Write-Host "  Application name   : $AppName"
Write-Host "  Authorization type : OAuth 2 authorization with a callback URL"
Write-Host "  Authorization callback URL : $callback   (copied to your clipboard)"
Write-Host "  Application website: (optional)"
Write-Host "  Email / Description: your email, e.g. 'Screen recorder that uploads clips'"
Write-Host ""
Set-Clipboard -Value $callback
Read-Host "Press Enter to open the form"
Start-Process $formUrl

Write-Host ""
Write-Host "Step 2: paste the credentials Imgur shows after you submit" -ForegroundColor Cyan
while ($true) {
    $clientId = (Read-Host "Client ID").Trim()
    if (-not $clientId) { continue }
    Write-Host "Checking with Imgur..." -NoNewline
    $credits = Test-ClientId $clientId
    if ($credits) {
        Write-Host " valid. App quota: $($credits.ClientRemaining)/$($credits.ClientLimit) requests left today." -ForegroundColor Green
        break
    }
    Write-Host " Imgur says this Client ID is invalid. Try again." -ForegroundColor Red
}

$secure = Read-Host "Client Secret" -AsSecureString
$clientSecret = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
    [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)).Trim()
if ($clientSecret.Length -lt 20) {
    Write-Warning "That secret looks short (Imgur secrets are usually 40 hex characters). It's only checked when you first log in."
}

@(
    "# Imgur API app compiled into screen2imgur by build.rs. Keep this file out of git."
    "SCREEN2IMGUR_CLIENT_ID=$clientId"
    "SCREEN2IMGUR_CLIENT_SECRET=$clientSecret"
) | Set-Content -Path $envFile -Encoding ascii

Write-Host ""
Write-Host "Saved $envFile" -ForegroundColor Green
Write-Host "Now build: cargo build --release"
Write-Host "Then run the exe, open Settings and click 'Log in with Imgur' to confirm the secret works."
