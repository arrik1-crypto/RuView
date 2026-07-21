package com.ruvnet.tactical

import android.annotation.SuppressLint
import android.bluetooth.BluetoothManager
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.os.Handler
import android.os.Looper
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.ConcurrentHashMap

/**
 * Scans for advertising BLE devices and posts them to the local server's
 * `/api/ble`. This is the phone-native **device** sensor for the auxiliary
 * overlay — it detects transmitting Bluetooth devices by RSSI, never people.
 *
 * Caller is responsible for holding the runtime scan permission before start().
 */
class BleScanner(private val context: Context, private val port: Int) {

    private val results = ConcurrentHashMap<String, ScanResult>()
    private val handler = Handler(Looper.getMainLooper())
    @Volatile private var scanning = false

    private val callback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult) {
            results[result.device.address] = result
        }
        override fun onBatchScanResults(rs: MutableList<ScanResult>) {
            for (r in rs) results[r.device.address] = r
        }
    }

    private val flushRunnable = object : Runnable {
        override fun run() {
            flush()
            if (scanning) handler.postDelayed(this, FLUSH_MS)
        }
    }

    @SuppressLint("MissingPermission")
    fun start() {
        if (scanning) return
        val mgr = context.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager ?: return
        val adapter = mgr.adapter ?: return
        if (!adapter.isEnabled) return
        val scanner = adapter.bluetoothLeScanner ?: return
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .build()
        try {
            scanner.startScan(null, settings, callback)
        } catch (e: SecurityException) {
            return
        }
        scanning = true
        handler.postDelayed(flushRunnable, FLUSH_MS)
    }

    @SuppressLint("MissingPermission")
    fun stop() {
        if (!scanning) return
        scanning = false
        handler.removeCallbacks(flushRunnable)
        try {
            val mgr = context.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
            mgr?.adapter?.bluetoothLeScanner?.stopScan(callback)
        } catch (_: SecurityException) {
        }
    }

    @SuppressLint("MissingPermission")
    private fun flush() {
        if (results.isEmpty()) return
        val snapshot = results.values.toList()
        results.clear()

        val arr = JSONArray()
        for (r in snapshot) {
            val o = JSONObject()
            o.put("id", r.device.address)
            val name = try {
                r.scanRecord?.deviceName
            } catch (_: SecurityException) {
                null
            }
            if (!name.isNullOrEmpty()) o.put("name", name)
            o.put("rssi", r.rssi)
            arr.put(o)
        }
        val body = JSONObject().put("devices", arr).toString()
        Thread { postJson(body) }.start()
    }

    private fun postJson(body: String) {
        try {
            val conn = URL("http://127.0.0.1:$port/api/ble").openConnection() as HttpURLConnection
            conn.requestMethod = "POST"
            conn.doOutput = true
            conn.connectTimeout = 2000
            conn.readTimeout = 2000
            conn.setRequestProperty("Content-Type", "application/json")
            conn.outputStream.use { it.write(body.toByteArray()) }
            conn.responseCode
            conn.disconnect()
        } catch (_: Exception) {
            // Server not up yet or transient — the next flush retries.
        }
    }

    companion object {
        private const val FLUSH_MS = 2000L
    }
}
