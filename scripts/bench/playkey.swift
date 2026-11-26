import AppKit

func post(_ down: Bool) {
    let flags = NSEvent.ModifierFlags(rawValue: down ? 0xA00 : 0xB00)
    let data1 = (16 << 16) | ((down ? 0xA : 0xB) << 8)
    let event = NSEvent.otherEvent(
        with: .systemDefined, location: .zero, modifierFlags: flags, timestamp: 0,
        windowNumber: 0, context: nil, subtype: 8, data1: data1, data2: -1)
    event?.cgEvent?.post(tap: .cghidEventTap)
}
post(true)
post(false)
