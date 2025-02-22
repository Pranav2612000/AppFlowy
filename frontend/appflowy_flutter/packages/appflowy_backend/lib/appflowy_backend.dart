export 'package:async/async.dart';
import 'dart:io';
import 'dart:async';
import 'package:appflowy_backend/rust_stream.dart';
import 'package:flutter/services.dart';
import 'dart:ffi';
import 'ffi.dart' as ffi;
import 'package:ffi/ffi.dart';

enum ExceptionType {
  AppearanceSettingsIsEmpty,
}

class FlowySDKException implements Exception {
  ExceptionType type;
  FlowySDKException(this.type);
}

class FlowySDK {
  static const MethodChannel _channel = MethodChannel('appflowy_backend');
  static Future<String> get platformVersion async {
    final String version = await _channel.invokeMethod('getPlatformVersion');
    return version;
  }

  FlowySDK();

  void dispose() {}

  Future<void> init(Directory sdkDir, String env) async {
    final port = RustStreamReceiver.shared.port;
    ffi.set_stream_port(port);

    ffi.store_dart_post_cobject(NativeApi.postCObject);
    ffi.set_env(env.toNativeUtf8());
    ffi.init_sdk(sdkDir.path.toNativeUtf8());
  }
}
