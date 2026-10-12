$ErrorActionPreference = 'Stop'
$private = Join-Path $PWD target/private-module-tests
New-Item -ItemType Directory -Force $private | Out-Null
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$acl = [System.Security.AccessControl.DirectorySecurity]::new()
$acl.SetOwner($identity.User)
$acl.SetAccessRuleProtection($true, $false)
$inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
  $identity.User,
  [System.Security.AccessControl.FileSystemRights]::FullControl,
  $inherit,
  [System.Security.AccessControl.PropagationFlags]::None,
  [System.Security.AccessControl.AccessControlType]::Allow
)
$acl.AddAccessRule($rule)
Set-Acl -LiteralPath $private -AclObject $acl
Copy-Item target/debug/examples/module_clock.dll $private
Copy-Item target/debug/examples/module_context.dll $private
Copy-Item target/debug/examples/module_context_eager.dll $private
Copy-Item target/debug/examples/module_host_callback.dll $private
Copy-Item target/debug/examples/module_controlled_init.dll $private
Copy-Item target/debug/examples/module_wrong_target.dll $private
# Admit every intentional fixture by its actual built bytes so later
# checks, including the wrong-target gate, can exercise their contract.
$fixtureAllowlist = @('module_clock.dll', 'module_context.dll', 'module_context_eager.dll', 'module_host_callback.dll', 'module_controlled_init.dll', 'module_wrong_target.dll') | ForEach-Object {
  $digest = (Get-FileHash -LiteralPath (Join-Path $private $_) -Algorithm SHA256).Hash.ToLowerInvariant()
  '"' + $_ + '" = "' + $digest + '"'
}
$fixtureAllowlist | Set-Content -LiteralPath (Join-Path $private modules.toml)
$clockTwoDirectory = Join-Path $private module-clock-two
New-Item -ItemType Directory -Force $clockTwoDirectory | Out-Null
Set-Acl -LiteralPath $clockTwoDirectory -AclObject $acl
Copy-Item target/debug/examples/module_clock_two.dll $clockTwoDirectory
$clockTwoHash = (Get-FileHash -LiteralPath (Join-Path $clockTwoDirectory module_clock_two.dll) -Algorithm SHA256).Hash.ToLowerInvariant()
'"module_clock_two.dll" = "' + $clockTwoHash + '"' | Set-Content -LiteralPath (Join-Path $clockTwoDirectory modules.toml) -NoNewline
$mixed = Join-Path $private mixed
New-Item -ItemType Directory -Force $mixed | Out-Null
Set-Acl -LiteralPath $mixed -AclObject $acl
Copy-Item target/debug/examples/module_clock.dll $mixed
Copy-Item target/debug/examples/module_wrong_target.dll $mixed
$module = (Get-ChildItem "$private/module_clock.dll" | Select-Object -First 1).FullName
$moduleTwo = (Get-ChildItem "$clockTwoDirectory/module_clock_two.dll" | Select-Object -First 1).FullName
$wrongTarget = (Get-ChildItem "$private/module_wrong_target.dll" | Select-Object -First 1).FullName
$env:TEMP = $private
$env:TMP = $private
$env:TINYBUS_TEST_CONTEXT_MODULE = (Get-Item "$private/module_context.dll").FullName
$env:TINYBUS_TEST_CONTEXT_EAGER_MODULE = (Get-Item "$private/module_context_eager.dll").FullName
$env:TINYBUS_TEST_CONTROLLED_INIT_MODULE = (Get-Item "$private/module_controlled_init.dll").FullName
$env:TINYBUS_TEST_HOST_CALLBACK_MODULE = (Get-Item "$private/module_host_callback.dll").FullName
$env:TINYBUS_TEST_MODULE = $module
$env:TINYBUS_TEST_MODULE_TWO = $moduleTwo
$env:TINYBUS_TEST_WRONG_TARGET = $wrongTarget
$env:TINYBUS_TEST_MODULE_DIRECTORY = $mixed
cargo test --locked --no-default-features --features modules,macros,test-support -- --ignored
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
# Directory admission against the real ACLs: a bundle Everyone can
# write falls back to the verified release cache.
cargo test --locked --no-default-features --features modules,macros admission_tests -- --include-ignored

if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
