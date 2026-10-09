package dev.gpui.mobile;

import android.app.Activity;
import android.content.ContentResolver;
import android.database.Cursor;
import android.provider.ContactsContract;

import java.util.ArrayList;

/**
 * JNI helper for reading the device address book via ContactsContract.
 *
 * Returns contacts as a flat {@code String[]}, so any text in a field arrives
 * intact. Per contact: id, displayName, givenName, familyName, the number of
 * phones followed by a number and a label for each, then the number of emails
 * followed by an address and a label for each.
 */
public final class GpuiContacts {

    /**
     * Get all contacts, sorted by display name.
     */
    public static String[] getContacts(Activity activity) {
        return queryContacts(activity, null, null);
    }

    /**
     * Search contacts by display name (case-insensitive LIKE match).
     */
    public static String[] searchContacts(Activity activity, String query) {
        return queryContacts(activity,
            ContactsContract.Contacts.DISPLAY_NAME_PRIMARY + " LIKE ?",
            new String[]{"%" + query + "%"});
    }

    /**
     * Get a single contact by its _ID.
     */
    public static String[] getContact(Activity activity, String id) {
        return queryContacts(activity,
            ContactsContract.Contacts._ID + " = ?",
            new String[]{id});
    }

    private static String[] queryContacts(Activity activity, String selection, String[] selectionArgs) {
        ContentResolver cr = activity.getContentResolver();
        ArrayList<String> result = new ArrayList<>();

        Cursor cursor = cr.query(
            ContactsContract.Contacts.CONTENT_URI,
            new String[]{
                ContactsContract.Contacts._ID,
                ContactsContract.Contacts.DISPLAY_NAME_PRIMARY,
            },
            selection, selectionArgs,
            ContactsContract.Contacts.DISPLAY_NAME_PRIMARY + " ASC"
        );

        if (cursor == null) return new String[0];

        try {
            while (cursor.moveToNext()) {
                String contactId = cursor.getString(0);
                String displayName = cursor.getString(1);
                if (displayName == null) displayName = "";

                // Get structured name
                String givenName = "";
                String familyName = "";
                Cursor nameCursor = cr.query(
                    ContactsContract.Data.CONTENT_URI,
                    new String[]{
                        ContactsContract.CommonDataKinds.StructuredName.GIVEN_NAME,
                        ContactsContract.CommonDataKinds.StructuredName.FAMILY_NAME,
                    },
                    ContactsContract.Data.CONTACT_ID + " = ? AND " +
                        ContactsContract.Data.MIMETYPE + " = ?",
                    new String[]{contactId, ContactsContract.CommonDataKinds.StructuredName.CONTENT_ITEM_TYPE},
                    null
                );
                if (nameCursor != null) {
                    try {
                        if (nameCursor.moveToFirst()) {
                            givenName = nameCursor.getString(0);
                            familyName = nameCursor.getString(1);
                            if (givenName == null) givenName = "";
                            if (familyName == null) familyName = "";
                        }
                    } finally {
                        nameCursor.close();
                    }
                }

                // Get phone numbers
                ArrayList<String> phones = new ArrayList<>();
                Cursor phoneCursor = cr.query(
                    ContactsContract.CommonDataKinds.Phone.CONTENT_URI,
                    new String[]{
                        ContactsContract.CommonDataKinds.Phone.NUMBER,
                        ContactsContract.CommonDataKinds.Phone.TYPE,
                    },
                    ContactsContract.CommonDataKinds.Phone.CONTACT_ID + " = ?",
                    new String[]{contactId}, null
                );
                if (phoneCursor != null) {
                    try {
                        while (phoneCursor.moveToNext()) {
                            String number = phoneCursor.getString(0);
                            int type = phoneCursor.getInt(1);
                            String label = ContactsContract.CommonDataKinds.Phone.getTypeLabel(
                                activity.getResources(), type, "other").toString();
                            phones.add(number != null ? number : "");
                            phones.add(label);
                        }
                    } finally {
                        phoneCursor.close();
                    }
                }

                // Get emails
                ArrayList<String> emails = new ArrayList<>();
                Cursor emailCursor = cr.query(
                    ContactsContract.CommonDataKinds.Email.CONTENT_URI,
                    new String[]{
                        ContactsContract.CommonDataKinds.Email.ADDRESS,
                        ContactsContract.CommonDataKinds.Email.TYPE,
                    },
                    ContactsContract.CommonDataKinds.Email.CONTACT_ID + " = ?",
                    new String[]{contactId}, null
                );
                if (emailCursor != null) {
                    try {
                        while (emailCursor.moveToNext()) {
                            String addr = emailCursor.getString(0);
                            int type = emailCursor.getInt(1);
                            String label = ContactsContract.CommonDataKinds.Email.getTypeLabel(
                                activity.getResources(), type, "other").toString();
                            emails.add(addr != null ? addr : "");
                            emails.add(label);
                        }
                    } finally {
                        emailCursor.close();
                    }
                }

                result.add(contactId);
                result.add(displayName);
                result.add(givenName);
                result.add(familyName);
                result.add(String.valueOf(phones.size() / 2));
                result.addAll(phones);
                result.add(String.valueOf(emails.size() / 2));
                result.addAll(emails);
            }
        } finally {
            cursor.close();
        }

        return result.toArray(new String[0]);
    }

    private GpuiContacts() {}
}
