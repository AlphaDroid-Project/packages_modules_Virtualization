/*
 * Copyright (C) 2021 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package com.android.microdroid.test.host;

import static com.android.tradefed.testtype.DeviceJUnit4ClassRunner.TestLogData;

import static com.google.common.truth.Truth.assertWithMessage;

import static org.junit.Assume.assumeTrue;

import com.android.compatibility.common.tradefed.build.CompatibilityBuildHelper;
import com.android.microdroid.test.common.DeviceProperties;
import com.android.microdroid.test.common.MetricsProcessor;
import com.android.tradefed.build.IBuildInfo;
import com.android.tradefed.device.DeviceNotAvailableException;
import com.android.tradefed.device.ITestDevice;
import com.android.tradefed.device.TestDevice;
import com.android.tradefed.testtype.junit4.BaseHostJUnit4Test;
import com.android.tradefed.util.CommandResult;
import com.android.tradefed.util.RunUtil;

import java.io.File;
import java.io.FileNotFoundException;
import java.util.Arrays;

public abstract class MicrodroidHostTestCaseBase extends BaseHostJUnit4Test {
    protected static final String TEST_ROOT = "/data/local/tmp/virt/";
    protected static final String LOG_PATH = TEST_ROOT + "log.txt";
    protected static final String CONSOLE_PATH = TEST_ROOT + "console.txt";
    private static final int TEST_VM_ADB_PORT = 8000;
    private static final String MICRODROID_SERIAL = "localhost:" + TEST_VM_ADB_PORT;
    private static final String INSTANCE_IMG = "instance.img";

    private static final long MICRODROID_ADB_CONNECT_TIMEOUT_MINUTES = 5;
    protected static final long MICRODROID_COMMAND_TIMEOUT_MILLIS = 30000;
    private static final long MICRODROID_COMMAND_RETRY_INTERVAL_MILLIS = 500;
    protected static final int MICRODROID_ADB_CONNECT_MAX_ATTEMPTS =
            (int) (MICRODROID_ADB_CONNECT_TIMEOUT_MINUTES * 60 * 1000
                / MICRODROID_COMMAND_RETRY_INTERVAL_MILLIS);

    public static void prepareVirtualizationTestSetup(ITestDevice androidDevice)
            throws DeviceNotAvailableException {
        CommandRunner android = new CommandRunner(androidDevice);

        // kill stale crosvm processes
        android.tryRun("killall", "crosvm");

        // disconnect from microdroid
        tryRunOnHost("adb", "disconnect", MICRODROID_SERIAL);

        // remove any leftover files under test root
        android.tryRun("rm", "-rf", TEST_ROOT + "*");

        android.tryRun("mkdir " + TEST_ROOT);
    }

    public static void cleanUpVirtualizationTestSetup(ITestDevice androidDevice)
            throws DeviceNotAvailableException {
        CommandRunner android = new CommandRunner(androidDevice);

        // disconnect from microdroid
        tryRunOnHost("adb", "disconnect", MICRODROID_SERIAL);

        // kill stale VMs and directories
        android.tryRun("killall", "crosvm");
        android.tryRun("stop", "virtualizationservice");
        android.tryRun("rm", "-rf", "/data/misc/virtualizationservice/*");
    }

    public boolean isUserBuild() {
        return DeviceProperties.create(getDevice()::getProperty).isUserBuild();
    }

    protected boolean isCuttlefish() {
        return DeviceProperties.create(getDevice()::getProperty).isCuttlefish();
    }

    protected boolean isHwasan() {
        return DeviceProperties.create(getDevice()::getProperty).isHwasan();
    }

    protected String getMetricPrefix() {
        return MetricsProcessor.getMetricPrefix(
                DeviceProperties.create(getDevice()::getProperty).getMetricsTag());
    }

    public static void assumeDeviceIsCapable(ITestDevice androidDevice) throws Exception {
        assumeTrue("Need an actual TestDevice", androidDevice instanceof TestDevice);
        TestDevice testDevice = (TestDevice) androidDevice;
        assumeTrue(
                "Requires VM support",
                testDevice.hasFeature("android.software.virtualization_framework"));
        assumeTrue("Requires VM support", testDevice.supportsMicrodroid());
    }

    public static void archiveLogThenDelete(TestLogData logs, ITestDevice device, String remotePath,
            String localName) throws DeviceNotAvailableException {
        LogArchiver.archiveLogThenDelete(logs, device, remotePath, localName);
    }

    public static void setPropertyOrThrow(ITestDevice device, String propertyName, String value)
            throws DeviceNotAvailableException {
        if (!device.setProperty(propertyName, value)) {
            throw new RuntimeException("Failed to set sysprop " + propertyName + " to " + value);
        }
    }

    // Run an arbitrary command in the host side and returns the result.
    // Note failure is not an error.
    public static String tryRunOnHost(String... cmd) {
        final long timeout = 10000;
        CommandResult result = RunUtil.getDefault().runTimedCmd(timeout, cmd);
        return result.getStdout().trim();
    }
    private static String join(String... strs) {
        return String.join(" ", Arrays.asList(strs));
    }

    public File findTestFile(String name) {
        return findTestFile(getBuild(), name);
    }

    private static File findTestFile(IBuildInfo buildInfo, String name) {
        try {
            return (new CompatibilityBuildHelper(buildInfo)).getTestFile(name);
        } catch (FileNotFoundException e) {
            throw new AssertionError("Missing test file: " + name, e);
        }
    }

    public String getPathForPackage(String packageName)
            throws DeviceNotAvailableException {
        return getPathForPackage(getDevice(), packageName);
    }

    // Get the path to the installed apk. Note that
    // getDevice().getAppPackageInfo(...).getCodePath() doesn't work due to the incorrect
    // parsing of the "=" character. (b/190975227). So we use the `pm path` command directly.
    private static String getPathForPackage(ITestDevice device, String packageName)
            throws DeviceNotAvailableException {
        CommandRunner android = new CommandRunner(device);
        String pathLine = android.run("pm", "path", packageName);
        assertWithMessage("Package " + packageName + " not found")
                .that(pathLine).startsWith("package:");
        return pathLine.substring("package:".length());
    }
}
