// Test transport only. The real system curl can reach only the fixture's loopback HTTP server.
using System;
using System.Diagnostics;
using System.IO;
public static class ReleaseCurlAdapter {
  public static int Main() {
    Console.In.ReadToEnd();
    string url = Environment.GetEnvironmentVariable("RELPROOF_CURL_URL");
    Uri endpoint;
    if (!Uri.TryCreate(url, UriKind.Absolute, out endpoint) || endpoint.Scheme != "http" || endpoint.Host != "127.0.0.1") return 90;
    var info = new ProcessStartInfo(Path.Combine(Environment.GetEnvironmentVariable("WINDIR"), "System32", "curl.exe"), "-sS -i --noproxy * --max-time 5 " + url);
    info.UseShellExecute = false;
    info.CreateNoWindow = true;
    info.RedirectStandardOutput = true;
    using (var child = Process.Start(info)) {
      Console.Write(child.StandardOutput.ReadToEnd());
      child.WaitForExit();
      return child.ExitCode;
    }
  }
}
