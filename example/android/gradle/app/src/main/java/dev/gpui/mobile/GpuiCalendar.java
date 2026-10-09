package dev.gpui.mobile;

import android.app.Activity;
import android.content.ContentResolver;
import android.content.ContentValues;
import android.database.Cursor;
import android.net.Uri;
import android.provider.CalendarContract;

import java.util.ArrayList;
import java.util.TimeZone;

/**
 * Calendar access for the calendar package. Failures, such as a SecurityException
 * without the calendar permissions, are thrown to the Rust caller.
 *
 * <p>Lists come back as flat {@code String[]}s, a fixed number of fields per row,
 * so any text in a field arrives intact.</p>
 */
public final class GpuiCalendar {

    /** Four fields per calendar: id, name, read-only ("1"/"0"), ARGB color. */
    public static String[] getCalendars(Activity activity) {
        ContentResolver cr = activity.getContentResolver();
        ArrayList<String> result = new ArrayList<>();

        Cursor cursor = null;
        try {
            cursor = cr.query(
                CalendarContract.Calendars.CONTENT_URI,
                new String[]{
                    CalendarContract.Calendars._ID,
                    CalendarContract.Calendars.CALENDAR_DISPLAY_NAME,
                    CalendarContract.Calendars.CALENDAR_ACCESS_LEVEL,
                    CalendarContract.Calendars.CALENDAR_COLOR,
                },
                null, null, null
            );

            if (cursor == null) return new String[0];
            while (cursor.moveToNext()) {
                int access = cursor.getInt(2);
                boolean readOnly = access < CalendarContract.Calendars.CAL_ACCESS_CONTRIBUTOR;
                result.add(cursor.getString(0));
                result.add(nullSafe(cursor.getString(1)));
                result.add(readOnly ? "1" : "0");
                result.add(String.valueOf(cursor.getInt(3) & 0xFFFFFFFFL));
            }
        } finally {
            if (cursor != null) cursor.close();
        }
        return result.toArray(new String[0]);
    }

    /**
     * Eight fields per event: id, title, description, location, start ms, end ms,
     * all-day ("1"/"0"), calendar id.
     */
    public static String[] getEvents(Activity activity, String calendarId, long startMs, long endMs) {
        ContentResolver cr = activity.getContentResolver();
        ArrayList<String> result = new ArrayList<>();

        String selection = CalendarContract.Events.CALENDAR_ID + " = ? AND " +
            CalendarContract.Events.DTSTART + " >= ? AND " +
            CalendarContract.Events.DTSTART + " <= ?";

        Cursor cursor = null;
        try {
            cursor = cr.query(
                CalendarContract.Events.CONTENT_URI,
                new String[]{
                    CalendarContract.Events._ID,
                    CalendarContract.Events.TITLE,
                    CalendarContract.Events.DESCRIPTION,
                    CalendarContract.Events.EVENT_LOCATION,
                    CalendarContract.Events.DTSTART,
                    CalendarContract.Events.DTEND,
                    CalendarContract.Events.ALL_DAY,
                    CalendarContract.Events.CALENDAR_ID,
                },
                selection,
                new String[]{calendarId, String.valueOf(startMs), String.valueOf(endMs)},
                CalendarContract.Events.DTSTART + " ASC"
            );

            if (cursor == null) return new String[0];
            while (cursor.moveToNext()) {
                result.add(cursor.getString(0));
                result.add(nullSafe(cursor.getString(1)));
                result.add(nullSafe(cursor.getString(2)));
                result.add(nullSafe(cursor.getString(3)));
                result.add(String.valueOf(cursor.getLong(4)));
                result.add(String.valueOf(cursor.getLong(5)));
                result.add(cursor.getInt(6) != 0 ? "1" : "0");
                result.add(cursor.getString(7));
            }
        } finally {
            if (cursor != null) cursor.close();
        }
        return result.toArray(new String[0]);
    }

    public static String createEvent(Activity activity, String calendarId,
            String title, String description, String location,
            long startMs, long endMs, boolean allDay) {
        ContentValues values = new ContentValues();
        values.put(CalendarContract.Events.CALENDAR_ID, Long.parseLong(calendarId));
        values.put(CalendarContract.Events.TITLE, title);
        values.put(CalendarContract.Events.DESCRIPTION, description);
        values.put(CalendarContract.Events.EVENT_LOCATION, location);
        values.put(CalendarContract.Events.DTSTART, startMs);
        values.put(CalendarContract.Events.DTEND, endMs);
        values.put(CalendarContract.Events.ALL_DAY, allDay ? 1 : 0);
        values.put(CalendarContract.Events.EVENT_TIMEZONE, TimeZone.getDefault().getID());

        Uri uri = activity.getContentResolver().insert(CalendarContract.Events.CONTENT_URI, values);
        return uri != null ? uri.getLastPathSegment() : null;
    }

    public static boolean deleteEvent(Activity activity, String eventId) {
        Uri uri = CalendarContract.Events.CONTENT_URI.buildUpon()
            .appendPath(eventId).build();
        return activity.getContentResolver().delete(uri, null, null) > 0;
    }

    private static String nullSafe(String s) { return s != null ? s : ""; }

    private GpuiCalendar() {}
}
