fn main() {
    // windows-targets 0.52 的 GNU 非 raw-dylib 分支使用聚合库名；显式补上真实系统导入库。
    if cfg!(target_os = "windows") && cfg!(target_env = "gnu") {
        println!("cargo:rustc-link-lib=shell32");
    }
    // 发布版要求管理员权限：MFT / USN Journal 需要；调试版保持 asInvoker 方便开发。
    // 注意：app_manifest 会整体替换默认清单，需保留 Common-Controls v6 依赖（TaskDialogIndirect）。
    let level = if cfg!(debug_assertions) {
        "asInvoker"
    } else {
        "requireAdministrator"
    };
    let manifest = format!(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="{level}" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>"#
    );
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(manifest)),
    )
    .expect("tauri build 失败");
}
