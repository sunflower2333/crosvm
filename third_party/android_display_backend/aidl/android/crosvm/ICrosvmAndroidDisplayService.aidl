/*
 * Copyright 2024 The Android Open Source Project
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

package android.crosvm;

import android.crosvm.DisplayConfig;
import android.os.ParcelFileDescriptor;
import android.view.Surface;

/**
 * Service to provide Crosvm with an Android Surface for showing a guest's
 * display.
 */
interface ICrosvmAndroidDisplayService {
    void setSurface(inout Surface surface, boolean forCursor);
    void setCursorStream(in ParcelFileDescriptor stream);
    void removeSurface(boolean forCursor);
    void saveFrameForSurface(boolean forCursor);
    void drawSavedFrameForSurface(boolean forCursor);

    /**
     * Current guest scanout configuration (single source of truth for the display size, updated on
     * every resolution change: UEFI modeset, guest boot, and guest runtime xrandr all flow through
     * the same configure() path). The app polls this to follow a mid-session resolution change.
     * Preserve its declaration position so its transaction code matches the app's copy.
     */
    DisplayConfig getDisplayConfig();

    /** Root Surface buffer-transform hint for the app-owned cursor; identity/90/180/270 only.
     * Appended for compatibility. Does not rotate the main scanout or change cursor position. */
    void setCursorTransformHint(int hint);

    /** Positive token for this Surface attachment. Requires the matching app/host pair. */
    long setSurfaceWithLease(in Surface surface, boolean forCursor);
    /** Stale Activity callbacks cannot detach a newer attachment. */
    void removeSurfaceWithLease(boolean forCursor, long lease);
    /** The asynchronous hint also belongs to a specific cursor attachment. */
    void setCursorTransformHintWithLease(int hint, long lease);

    /** Independent main-root observation; unknown/reflection hints revoke the profile. */
    void setMainTransformHintWithLease(int hint, long lease, long rootGeneration, long serial);
}
