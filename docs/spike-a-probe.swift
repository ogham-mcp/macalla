import Foundation
import Speech

@main
struct Probe {
    static func main() async {
        print("macOS:", ProcessInfo.processInfo.operatingSystemVersionString)

        print("SpeechTranscriber.isAvailable:", SpeechTranscriber.isAvailable)

        let supported = await SpeechTranscriber.supportedLocales
        print("supportedLocales (\(supported.count)):")
        for l in supported.sorted(by: { $0.identifier < $1.identifier }) {
            print("   ", l.identifier)
        }

        let installed = await SpeechTranscriber.installedLocales
        print("installedLocales (\(installed.count)):",
              installed.map(\.identifier).sorted().joined(separator: ", "))

        // Can we actually construct a transcriber and ask about its assets?
        let t = SpeechTranscriber(locale: Locale(identifier: "en-US"),
                                  preset: .progressiveTranscription)
        let status = await AssetInventory.status(forModules: [t])
        print("AssetInventory.status(en-US):", String(describing: status))

        print("reservedLocales:", await AssetInventory.reservedLocales.map(\.identifier))
        print("maximumReservedLocales:", await AssetInventory.maximumReservedLocales)
    }
}
