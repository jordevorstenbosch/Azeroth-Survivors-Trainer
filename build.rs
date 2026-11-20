fn main() {
    const ICON: &str = "assets/trainer-icon.ico";
    println!("cargo:rerun-if-changed={ICON}");
    winresource::WindowsResource::new()
        .set_icon(ICON)
        .set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df"
        language="*" />
    </dependentAssembly>
  </dependency>
</assembly>"#,
        )
        .compile()
        .expect("embed Windows trainer icon");
}
