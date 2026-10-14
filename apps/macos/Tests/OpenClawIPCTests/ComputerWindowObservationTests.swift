import CoreGraphics
import Darwin
import Foundation
import OpenClawKit
import PeekabooAutomationKit
import Testing
@testable import OpenClaw

@MainActor
struct ComputerWindowObservationTests {
    @Test func `window executor rejects screenshot omission before capture`() async {
        let service = ComputerWindowActionExecutor()
        do {
            _ = try await service.perform(
                OpenClawComputerActParams(action: .getWindowState, windowRef: "window-1", includeScreenshot: false),
                lifecycleGeneration: 0,
                checkExecutionAllowed: {})
            Issue.record("Expected unsupported screenshot omission")
        } catch {
            #expect(error.localizedDescription.contains("includeScreenshot:false is unsupported by Peekaboo"))
        }
    }

    @Test func `window state observation requests an owned snapshot in the executor directory`() {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("window-state-request")
        let budget = AXTraversalBudget(maxDepth: 8, maxElementCount: 250)
        let request = ComputerWindowActionExecutor.windowStateObservationRequest(
            windowID: 42,
            traversalBudget: budget,
            outputDirectory: directory)

        #expect(request.output.saveSnapshot)
        #expect(request.output.snapshotID == nil)
        #expect(request.output.path == directory.path + "/")
        #expect(request.target == .windowID(42))
        #expect(request.capture.focus == .background)
        #expect(request.detection.mode == .accessibility)
        #expect(request.detection.traversalBudget == budget)
    }

    @Test func `owned window snapshot publishes its capture and deletes it on cleanup`() async throws {
        let directory = try Self.makeDirectory()
        defer { WindowObservationArtifacts.removeAll(in: directory) }
        let manager = InMemorySnapshotManager(options: ComputerWindowActionExecutor.snapshotManagerOptions)
        let snapshotID = try await manager.createSnapshot(pendingAt: Date())

        let write = try await ObservationOutputWriter(snapshotManager: manager).write(
            capture: Self.capture(),
            elements: nil,
            options: DesktopObservationOutputOptions(
                path: directory.path + "/",
                saveSnapshot: true,
                snapshotID: snapshotID))

        let capturePath = try #require(write.files.rawScreenshotPath)
        #expect(write.files.publishedSnapshotID == snapshotID)
        #expect(capturePath.hasPrefix(directory.path + "/"))
        let stored = try await manager.getUIAutomationSnapshot(snapshotId: snapshotID)
        #expect(stored?.screenshotPath == capturePath)

        try await manager.cleanSnapshot(snapshotId: snapshotID)
        #expect(!FileManager.default.fileExists(atPath: capturePath))
    }

    @Test func `failed observation sweep removes only captures written since it started`() async throws {
        let directory = try Self.makeDirectory()
        defer { WindowObservationArtifacts.removeAll(in: directory) }
        let earlier = directory.appendingPathComponent("earlier.png")
        try Data([0x89, 0x50]).write(to: earlier)
        try FileManager.default.setAttributes(
            [.modificationDate: Date(timeIntervalSinceNow: -60)],
            ofItemAtPath: earlier.path)

        let startedAt = Date()
        // The writer persists the capture before publication; without a snapshot
        // it stays unregistered, the shape a failed observation leaves behind.
        let write = try await ObservationOutputWriter().write(
            capture: Self.capture(),
            elements: nil,
            options: DesktopObservationOutputOptions(path: directory.path + "/", saveRawScreenshot: true))
        let orphan = try #require(write.files.rawScreenshotPath)

        WindowObservationArtifacts.discardArtifacts(writtenSince: startedAt, in: directory)

        #expect(!FileManager.default.fileExists(atPath: orphan))
        #expect(FileManager.default.fileExists(atPath: earlier.path))
    }

    @Test func `interrupted runs lose their captures while running processes keep theirs`() throws {
        let root = try Self.makeDirectory()
        defer { WindowObservationArtifacts.removeAll(in: root) }
        // PID 1 always runs; macOS never assigns a PID above 99998.
        for name in ["\(getpid())", "1", "999999", "not-a-pid"] {
            let directory = root.appendingPathComponent(name, isDirectory: true)
            try WindowObservationArtifacts.createPrivateDirectory(directory)
            try Data([0x89, 0x50]).write(to: directory.appendingPathComponent("capture.png"))
        }

        WindowObservationArtifacts.removeInterruptedRuns(in: root)

        let remaining = try FileManager.default.contentsOfDirectory(atPath: root.path).sorted()
        #expect(remaining == ["\(getpid())", "1"].sorted())
    }

    @Test func `capture directories are private to the user`() throws {
        let root = try Self.makeDirectory()
        defer { WindowObservationArtifacts.removeAll(in: root) }
        let parent = root.appendingPathComponent("\(getpid())", isDirectory: true)
        let directory = parent.appendingPathComponent("executor", isDirectory: true)

        try WindowObservationArtifacts.createPrivateDirectory(directory)

        for url in [parent, directory] {
            let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
            #expect((attributes[.posixPermissions] as? NSNumber)?.intValue == 0o700)
        }
    }

    private static func makeDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("openclaw-window-observation-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    private static func capture() -> CaptureResult {
        CaptureResult(
            imageData: Data([0x89, 0x50, 0x4E, 0x47]),
            metadata: CaptureMetadata(size: CGSize(width: 4, height: 4), mode: .window))
    }
}
