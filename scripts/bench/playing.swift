import CoreAudio

func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
}

var listAddress = address(kAudioHardwarePropertyProcessObjectList)
var size: UInt32 = 0
AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &listAddress, 0, nil, &size)
var objects = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &listAddress, 0, nil, &size, &objects)
for object in objects {
    var pidAddress = address(kAudioProcessPropertyPID)
    var pid: pid_t = 0
    var pidSize = UInt32(MemoryLayout<pid_t>.size)
    AudioObjectGetPropertyData(object, &pidAddress, 0, nil, &pidSize, &pid)
    var runAddress = address(kAudioProcessPropertyIsRunningOutput)
    var running: UInt32 = 0
    var runSize = UInt32(MemoryLayout<UInt32>.size)
    AudioObjectGetPropertyData(object, &runAddress, 0, nil, &runSize, &running)
    if running != 0 { print(pid) }
}
