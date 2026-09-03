// fm-shim — a dependency-free CLI bridge to Apple's on-device Foundation Models.
//
// Build:  swiftc -O -target arm64-apple-macos26.0 -framework FoundationModels -o fm-shim fm-shim.swift
//
// Protocol
//   stdin  : {"messages":[{"role":"system"|"user"|"assistant","content":"..."}],
//             "stream":bool, "format":"ndjson"|"text",
//             "temperature":Double?, "maxTokens":Int?}
//   stdout : stream=false -> the response text
//            stream=true, format=text   -> raw token deltas
//            stream=true, format=ndjson -> {"type":"delta","text":...} lines, then {"type":"done",...}
//   stderr : one machine-readable status line, `fm-status: <status>`, always emitted first.
//   exit   : 0 ok; 10-13 model unavailable; 20-29 generation error; 2 bad input; 30 other.
//
// The availability probe runs before stdin is read, so `--check` costs no input and the
// caller can distinguish "this Mac cannot serve the model" from "the request failed".

import Foundation
import FoundationModels

// MARK: - Exit codes

enum Exit: Int32 {
    case ok = 0
    case badInput = 2

    case deviceNotEligible = 10
    case appleIntelligenceNotEnabled = 11
    case modelNotReady = 12
    case unavailableUnknown = 13

    case exceededContextWindowSize = 20
    case guardrailViolation = 21
    case refusal = 22
    case rateLimited = 23
    case concurrentRequests = 24
    case unsupportedLanguageOrLocale = 25
    case assetsUnavailable = 26
    case unsupportedGuide = 27
    case decodingFailure = 28
    case generationOther = 29

    case other = 30
}

// MARK: - IO helpers

let stdoutHandle = FileHandle.standardOutput
let stderrHandle = FileHandle.standardError

func emitOut(_ s: String) {
    if let d = s.data(using: .utf8) { stdoutHandle.write(d) }
}

func emitErr(_ s: String) {
    if let d = (s + "\n").data(using: .utf8) { stderrHandle.write(d) }
}

/// JSON-encode a single value so NDJSON frames never need hand-rolled escaping.
func jsonString(_ s: String) -> String {
    let data = try! JSONSerialization.data(withJSONObject: [s], options: [])
    var text = String(data: data, encoding: .utf8)!
    text.removeFirst()  // [
    text.removeLast()   // ]
    return text
}

func die(_ code: Exit, _ message: String) -> Never {
    emitErr("fm-error: \(message)")
    exit(code.rawValue)
}

// MARK: - Request parsing

struct Message {
    let role: String
    let content: String
}

struct Request {
    var messages: [Message] = []
    var stream = false
    var ndjson = true
    var temperature: Double?
    var maxTokens: Int?
}

func parseRequest(_ data: Data) -> Request {
    guard !data.isEmpty else { die(.badInput, "empty stdin; expected a JSON request object") }
    guard let root = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        die(.badInput, "stdin is not a JSON object")
    }
    guard let rawMessages = root["messages"] as? [[String: Any]], !rawMessages.isEmpty else {
        die(.badInput, "missing or empty \"messages\" array")
    }

    var req = Request()
    for (i, m) in rawMessages.enumerated() {
        guard let role = m["role"] as? String, let content = m["content"] as? String else {
            die(.badInput, "messages[\(i)] needs string \"role\" and \"content\"")
        }
        guard ["system", "user", "assistant"].contains(role) else {
            die(.badInput, "messages[\(i)] has unknown role \"\(role)\"")
        }
        req.messages.append(Message(role: role, content: content))
    }
    req.stream = (root["stream"] as? Bool) ?? false
    if let format = root["format"] as? String {
        guard ["ndjson", "text"].contains(format) else {
            die(.badInput, "\"format\" must be \"ndjson\" or \"text\"")
        }
        req.ndjson = (format == "ndjson")
    }
    req.temperature = root["temperature"] as? Double
    req.maxTokens = root["maxTokens"] as? Int
    return req
}

// MARK: - Availability

/// Probes the OS model and exits non-zero when it cannot serve a request.
/// Returns the live context-window size so the caller can budget its prompt.
func requireAvailableModel() -> (model: SystemLanguageModel, contextSize: Int) {
    let model = SystemLanguageModel.default
    switch model.availability {
    case .available:
        emitErr("fm-status: available")
        return (model, model.contextSize)
    case .unavailable(let reason):
        switch reason {
        case .deviceNotEligible:
            emitErr("fm-status: unavailable:deviceNotEligible")
            exit(Exit.deviceNotEligible.rawValue)
        case .appleIntelligenceNotEnabled:
            emitErr("fm-status: unavailable:appleIntelligenceNotEnabled")
            exit(Exit.appleIntelligenceNotEnabled.rawValue)
        case .modelNotReady:
            emitErr("fm-status: unavailable:modelNotReady")
            exit(Exit.modelNotReady.rawValue)
        // UnavailableReason is not @frozen: a future OS can add a case, and a shim
        // that trapped here would break on an OS update it never shipped against.
        @unknown default:
            emitErr("fm-status: unavailable:unknownReason")
            exit(Exit.unavailableUnknown.rawValue)
        }
    // Availability itself IS @frozen; the arm is defensive only.
    @unknown default:
        emitErr("fm-status: unknownCase")
        exit(Exit.unavailableUnknown.rawValue)
    }
}

// MARK: - Session construction

/// Maps the OpenAI-shaped message list onto a Transcript: system messages become the
/// instructions entry, every turn before the trailing user message becomes history, and
/// that trailing message is returned as the prompt to generate against.
func buildSession(_ req: Request, model: SystemLanguageModel) -> (LanguageModelSession, String) {
    guard let last = req.messages.last, last.role == "user" else {
        die(.badInput, "the final message must have role \"user\"")
    }
    let history = req.messages.dropLast()

    let systemText = history.filter { $0.role == "system" }.map(\.content)
        .joined(separator: "\n\n")

    var entries: [Transcript.Entry] = []
    if !systemText.isEmpty {
        entries.append(.instructions(.init(
            segments: [.text(.init(content: systemText))],
            toolDefinitions: []
        )))
    }
    for m in history {
        switch m.role {
        case "user":
            entries.append(.prompt(.init(segments: [.text(.init(content: m.content))])))
        case "assistant":
            entries.append(.response(.init(assetIDs: [], segments: [.text(.init(content: m.content))])))
        default:
            break  // system messages already folded into the instructions entry
        }
    }

    let session = entries.isEmpty
        ? LanguageModelSession(model: model)
        : LanguageModelSession(model: model, transcript: Transcript(entries: entries))
    return (session, last.content)
}

func classify(_ error: any Error) -> (Exit, String) {
    guard let gen = error as? LanguageModelSession.GenerationError else {
        return (.other, String(describing: error))
    }
    switch gen {
    case .exceededContextWindowSize(let c): return (.exceededContextWindowSize, c.debugDescription)
    case .assetsUnavailable(let c): return (.assetsUnavailable, c.debugDescription)
    case .guardrailViolation(let c): return (.guardrailViolation, c.debugDescription)
    case .unsupportedGuide(let c): return (.unsupportedGuide, c.debugDescription)
    case .unsupportedLanguageOrLocale(let c): return (.unsupportedLanguageOrLocale, c.debugDescription)
    case .decodingFailure(let c): return (.decodingFailure, c.debugDescription)
    case .rateLimited(let c): return (.rateLimited, c.debugDescription)
    case .concurrentRequests(let c): return (.concurrentRequests, c.debugDescription)
    case .refusal(_, let c): return (.refusal, c.debugDescription)
    @unknown default: return (.generationOther, String(describing: gen))
    }
}

// MARK: - Entry point

@main
struct FMShim {
    static func main() async {
        let args = Array(CommandLine.arguments.dropFirst())

        if args.contains("--help") || args.contains("-h") {
            emitOut("""
                usage: fm-shim [--check]
                  reads a JSON request on stdin, writes the model response on stdout.
                  --check  probe availability only; print the status line and exit.
                """ + "\n")
            exit(Exit.ok.rawValue)
        }

        let checkOnly = args.contains("--check")
        let (model, contextSize) = requireAvailableModel()
        emitErr("fm-context-size: \(contextSize)")
        if checkOnly { exit(Exit.ok.rawValue) }

        let req = parseRequest(FileHandle.standardInput.readDataToEndOfFile())
        let (session, prompt) = buildSession(req, model: model)
        let options = GenerationOptions(
            temperature: req.temperature,
            maximumResponseTokens: req.maxTokens
        )

        let start = DispatchTime.now()
        func elapsedMs(since t: DispatchTime) -> Double {
            Double(DispatchTime.now().uptimeNanoseconds - t.uptimeNanoseconds) / 1_000_000
        }

        do {
            if req.stream {
                var emitted = ""
                var firstTokenMs: Double?
                // Snapshots are CUMULATIVE for a String response, so a delta protocol
                // has to diff against what was already written rather than forward the
                // snapshot. Common-prefix rather than a plain suffix cut: the model may
                // revise earlier characters between snapshots.
                for try await snapshot in session.streamResponse(to: prompt, options: options) {
                    let text = snapshot.content
                    if firstTokenMs == nil { firstTokenMs = elapsedMs(since: start) }
                    let common = zip(emitted, text).prefix { $0 == $1 }.count
                    if common < emitted.count {
                        // A revision: restate the whole snapshot so the consumer stays correct.
                        if req.ndjson {
                            emitOut("{\"type\":\"reset\",\"text\":\(jsonString(text))}\n")
                        } else {
                            emitOut(String(text.dropFirst(common)))
                        }
                    } else if text.count > common {
                        let delta = String(text.dropFirst(common))
                        if req.ndjson {
                            emitOut("{\"type\":\"delta\",\"text\":\(jsonString(delta))}\n")
                        } else {
                            emitOut(delta)
                        }
                    }
                    emitted = text
                }
                let totalMs = elapsedMs(since: start)
                if req.ndjson {
                    emitOut("""
                        {"type":"done","text":\(jsonString(emitted)),\
                        "firstTokenMs":\(firstTokenMs ?? totalMs),"totalMs":\(totalMs)}
                        """ + "\n")
                }
                emitErr("fm-timing: firstTokenMs=\(firstTokenMs ?? totalMs) totalMs=\(totalMs)")
            } else {
                let response = try await session.respond(to: prompt, options: options)
                emitOut(response.content)
                emitErr("fm-timing: totalMs=\(elapsedMs(since: start))")
            }
        } catch {
            let (code, detail) = classify(error)
            emitErr("fm-error: \(detail)")
            exit(code.rawValue)
        }

        exit(Exit.ok.rawValue)
    }
}
